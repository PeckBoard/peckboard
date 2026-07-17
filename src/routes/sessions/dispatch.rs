use axum::{
    Extension, Json, extract::Path, extract::State, http::StatusCode, response::IntoResponse,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::auth::middleware::AuthUser;
use crate::db::models::UpdateSession;
use crate::provider::message::{UserAttachment, UserMessage};
use crate::provider::stream::SpawnConfig;
use crate::routes::attachments::load_attachment_payload;
use crate::state::AppState;

use super::resolve_references;

/// Build a compact transcript of a chat session's prior turns for the
/// pre-hatch hook payload: user messages and agent replies only, oldest-
/// first, capped so a long conversation can't blow the cheap research
/// model's context. Keeps the most RECENT turns within the char budget and
/// drops the oldest; returns an empty string when there's nothing to send.
async fn build_prehatch_history(state: &Arc<AppState>, session_id: &str) -> String {
    const MAX_EVENTS: i64 = 200;
    const MAX_CHARS: usize = 8000;
    const MAX_TURN_CHARS: usize = 2000;
    let events = match state.db.events_tail(session_id, MAX_EVENTS).await {
        Ok(e) => e,
        Err(_) => return String::new(),
    };
    let mut turns: Vec<String> = Vec::new();
    for ev in &events {
        let role = match ev.kind.as_str() {
            "user" => "User",
            "agent-text" => "Assistant",
            _ => continue,
        };
        let data: serde_json::Value = serde_json::from_str(&ev.data).unwrap_or_default();
        let text = data
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .trim();
        if text.is_empty() {
            continue;
        }
        let clipped = if text.chars().count() > MAX_TURN_CHARS {
            format!(
                "{}\u{2026}",
                text.chars().take(MAX_TURN_CHARS).collect::<String>()
            )
        } else {
            text.to_string()
        };
        turns.push(format!("{role}: {clipped}"));
    }
    let mut total = 0usize;
    let mut kept: Vec<String> = Vec::new();
    for turn in turns.iter().rev() {
        total += turn.chars().count() + 2;
        if total > MAX_CHARS {
            break;
        }
        kept.push(turn.clone());
    }
    kept.reverse();
    kept.join("\n\n")
}

#[derive(Deserialize)]
pub(super) struct SendMessageRequest {
    text: String,
    #[serde(default, rename = "attachmentIds")]
    attachment_ids: Option<Vec<String>>,
    model: Option<String>,
    effort: Option<String>,
}

/// POST /api/sessions/:id/message -- send a message to spawn a Claude CLI process.
/// Appends a user event, spawns the CLI in the background (which emits its own
/// agent-start event via the stream parser), and returns 200 immediately.
pub(super) async fn send_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Extension(user): Extension<AuthUser>,
    Json(body): Json<SendMessageRequest>,
) -> impl IntoResponse {
    tracing::info!(session_id = %id, "Sending message");
    // Verify session exists
    let session = state.db.get_session(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    let session = match session {
        Some(s) => s,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "session not found" })),
            ));
        }
    };

    // A model-switch handover or context compaction is mid-flight: the
    // outgoing model is still writing its doc. Refuse new user turns until
    // it lands so we don't contaminate the doc-generation turn or race the
    // model flip / conversation reset.
    if session.handover_to_model.is_some() {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "handover or context compaction in progress; try again in a moment",
            })),
        ));
    }

    let attachment_ids = body.attachment_ids.clone();

    // Resolve [session:id] and [report:folder/file] references early; both
    // the queued and started paths use the resolved text.
    let mut resolved_text = resolve_references(&body.text, &state).await;

    // Build spawn config — resolve model/effort with precedence:
    //   request body > session > card > project > "default"
    let (resolved_model, resolved_effort) = {
        let mut model: Option<String> = body.model;
        let mut effort: Option<String> = body.effort;

        if model.is_none() {
            model = session.model.clone();
        }
        if effort.is_none() {
            effort = session.effort.clone();
        }

        if model.is_none() || effort.is_none() {
            if let Some(ref card_id) = session.card_id {
                if let Ok(Some(card)) = state.db.get_card(card_id).await {
                    if model.is_none() {
                        model = card.model.clone();
                    }
                    if effort.is_none() {
                        effort = card.effort.clone();
                    }

                    if model.is_none() || effort.is_none() {
                        if let Ok(Some(project)) = state.db.get_project(&card.project_id).await {
                            if model.is_none() {
                                model = project.model.clone();
                            }
                            if effort.is_none() {
                                effort = project.effort.clone();
                            }
                        }
                    }
                }
            }
        }

        (model.unwrap_or_else(|| "default".into()), effort)
    };

    // Mark any pending question events as dismissed before appending
    // the user's message. Otherwise the question card stays rendered
    // in the chat after the user has clearly chosen to type past it,
    // and the agent gets the new user input with no signal that the
    // earlier question is no longer outstanding. The dismissal mirrors
    // the explicit reject path (the question UI's "Skip" button) — but
    // we persist + broadcast directly here rather than going through
    // the `/events` route, to avoid the route's `question-resolved`
    // side effect that respawns the agent (this turn is about to do
    // exactly that with the user's actual text).
    dismiss_pending_questions(&state, &id).await;

    // Pre-warm hook: let a plugin intercept an interactive chat message
    // before it reaches the agent (e.g. the pre-hatcher plugin enriches it
    // with context gathered by a cheaper model). Chats only — workers and
    // experts run orchestrated prompts — and only plain text turns
    // (attachments pass straight through). Allow-with-payload rewrites the
    // text inline; Cancel means the plugin took ownership of the turn: core
    // appends a `pre-hatch` placeholder event (the UI renders it as the
    // pending user bubble with a live feed of the research session, keyed
    // by the verdict data's `temp_session_id`) and does NOT dispatch — the
    // plugin appends the final `user` event and resumes the session when
    // its enrichment lands.
    if !session.is_worker
        && !session.is_expert
        && attachment_ids.as_deref().map_or(true, |a| a.is_empty())
        && state
            .plugins
            .has_listeners(crate::plugin::hooks::MESSAGE_BEFORE_HOOK)
            .await
    {
        let (provider_id, _) = crate::provider::registry::ProviderRegistry::parse_model_id(
            &resolved_model,
            crate::provider::manager::DEFAULT_PROVIDER,
        );
        // The model the pre-hatcher researches on: the user's Settings
        // override when set, otherwise the provider's cheapest priced model.
        let cheap_model = match crate::routes::settings::pre_hatcher_model(&state).await {
            Some(m) => Some(m),
            None => state
                .provider_registry
                .cheapest_model(&provider_id)
                .await
                .map(|m| format!("{provider_id}:{m}")),
        };
        // The chat's prior turns, so the pre-hatcher researches with the
        // whole conversation in view, and the configurable research system
        // prompt (a library name, default "fable 5") resolved to its body.
        let history = build_prehatch_history(&state, &id).await;
        let sp_name = crate::routes::settings::pre_hatcher_system_prompt_name(&state).await;
        let system_prompt = state
            .db
            .resolve_system_prompt(Some(&sp_name))
            .await
            .ok()
            .flatten();
        let hook_result = state
            .plugins
            .dispatch_scoped(
                crate::plugin::hooks::MESSAGE_BEFORE_HOOK,
                &user.user_id,
                Some(session.folder_id.clone()),
                session.project_id.clone(),
                Some(id.clone()),
                serde_json::json!({
                    "session_id": id,
                    "text": resolved_text,
                    "model": resolved_model,
                    "effort": resolved_effort,
                    "cheap_model": cheap_model,
                    "history": history,
                    "system_prompt": system_prompt.as_ref().map(|(_, body)| body.clone()),
                    "system_prompt_name": system_prompt.as_ref().map(|(name, _)| name.clone()),
                }),
            )
            .await;
        match hook_result {
            crate::plugin::hooks::HookResult::Cancelled {
                plugin,
                reason,
                data,
            } => {
                let ev = state
                    .db
                    .append_event(&id, "pre-hatch", {
                        let mut ev_data = serde_json::json!({
                            "text": resolved_text,
                            "plugin": plugin,
                            "reason": reason,
                        });
                        // The pre-hatcher's cancel data carries
                        // `temp_session_id` + `model` so the UI can
                        // stream the research session's actions into
                        // the parked bubble.
                        if let Some(serde_json::Value::Object(extra)) = data {
                            if let Some(obj) = ev_data.as_object_mut() {
                                for (k, v) in extra {
                                    obj.entry(k).or_insert(v);
                                }
                            }
                        }
                        ev_data
                    })
                    .await
                    .map_err(|e| {
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(serde_json::json!({ "error": e.to_string() })),
                        )
                    })?;
                state
                    .broadcaster
                    .broadcast(crate::ws::broadcaster::WsEvent {
                        event_type: "event".into(),
                        session_id: id.clone(),
                        data: serde_json::json!({
                            "id": ev.id,
                            "seq": ev.seq,
                            "ts": ev.ts,
                            "kind": ev.kind,
                            "data": serde_json::from_str::<serde_json::Value>(&ev.data)
                                .unwrap_or_default(),
                        }),
                    });
                let now = chrono::Utc::now().to_rfc3339();
                let _ = state
                    .db
                    .update_session(
                        &id,
                        UpdateSession {
                            last_activity: Some(now),
                            ..Default::default()
                        },
                    )
                    .await;
                return Ok(Json(serde_json::json!({
                    "status": "pre-hatching",
                    "session_id": id,
                    "plugin": plugin,
                })));
            }
            crate::plugin::hooks::HookResult::Allowed(p) => {
                if let Some(t) = p.get("text").and_then(|v| v.as_str())
                    && t != resolved_text
                {
                    resolved_text = t.to_string();
                }
            }
        }
    }

    // Resolve attachment IDs to bytes BEFORE appending the user event:
    // the provider context owns the payload (move, not borrow), so doing
    // the disk reads here means the provider's mid-stream injection path
    // never has to reach back into the attachments dir. Doing it up front
    // also lets us record each attachment's filename + mime on the user
    // event, so the chat UI can show "image attached" on the bubble for
    // every provider — not just the ones that forward the bytes to the
    // model. Unknown/missing ids drop with a warning rather than failing
    // the send — losing a stale id should not throw away the rest of the
    // message.
    let user_attachments = load_attachments(&id, attachment_ids.as_deref(), &state).await;

    // Always append the user event up front so the chat transcript
    // reflects the order the user typed in, regardless of whether the
    // agent is mid-turn or idle. In stream-json mode the Claude CLI
    // accepts new user envelopes on stdin at any time and consumes
    // them after the current `result` — there is no peckboard-layer
    // queue to gate this on.
    let mut user_data = serde_json::json!({ "text": resolved_text });
    if let Some(ref ids) = attachment_ids {
        user_data["attachmentIds"] = serde_json::json!(ids);
    }
    // Lightweight metadata (filename + mime) for the FE to render an
    // attachment indicator on the bubble. Derived from the bytes we just
    // loaded, so it only lists attachments that actually resolved.
    if !user_attachments.is_empty() {
        user_data["attachments"] = serde_json::json!(
            user_attachments
                .iter()
                .map(|a| serde_json::json!({
                    "filename": a.filename,
                    "mime_type": a.mime_type,
                }))
                .collect::<Vec<_>>()
        );
    }

    let user_event = state
        .db
        .append_event(&id, "user", user_data)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    state.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
        event_type: "event".into(),
        session_id: id.clone(),
        data: serde_json::json!({
            "id": user_event.id,
            "seq": user_event.seq,
            "ts": user_event.ts,
            "kind": user_event.kind,
            "data": serde_json::from_str::<serde_json::Value>(&user_event.data).unwrap_or_default(),
        }),
    });

    let now = chrono::Utc::now().to_rfc3339();
    let _ = state
        .db
        .update_session(
            &id,
            UpdateSession {
                last_activity: Some(now),
                ..Default::default()
            },
        )
        .await;

    let mcp_token = state
        .mcp_tokens
        .issue_token(id.clone(), session.project_id.clone())
        .await;

    let mcp_config_path = crate::service::mcp_server::write_mcp_config(
        &state.config.data_dir,
        &id,
        state.config.port,
        &mcp_token,
    )
    .ok()
    .map(|p| p.to_string_lossy().to_string());

    let config = SpawnConfig {
        model: resolved_model,
        effort: resolved_effort,
        working_dir: String::new(),
        mcp_config_path,
        env: Default::default(),
        permission_mode: Some("bypass".into()),
        timeout_ms: None,
        metadata: serde_json::Value::Null,
        system_prompt_suffix: None,
        system_prompt_override: None,
        // Populated in SessionManager::final_config from the plugin registry.
        extra_allowed_tools: Vec::new(),
        extra_disallowed_tools: Vec::new(),
        // Set from the session row in SessionManager::final_config.
        is_worker: false,
        is_pre_hatcher: false,
    };

    // Any pending handover/compaction doc is injected inside
    // `send_message_locked` (the single dispatch chokepoint), so the text
    // goes out as typed here. The user event above already recorded it.
    let dispatch_text = resolved_text;

    // `send_or_queue` acquires the per-session lock internally,
    // dispatches through the long-lived child (spawning lazily on
    // the first turn) and returns `Queued` iff the agent was
    // already mid-turn when the bytes hit stdin.
    let outcome = state
        .session_manager
        .send_or_queue(
            &id,
            UserMessage {
                text: dispatch_text,
                attachments: user_attachments,
            },
            &state.db,
            &state.broadcaster,
            config,
        )
        .await;

    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => {
            tracing::error!(session_id = %id, "Failed to dispatch message: {}", e);
            let crash_event = state
                .db
                .append_event(
                    &id,
                    "agent-end",
                    serde_json::json!({
                        "status": "crashed",
                        "reason": format!("dispatch error: {}", e),
                    }),
                )
                .await;
            if let Ok(ev) = crash_event {
                state.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
                    event_type: "event".into(),
                    session_id: id.clone(),
                    data: serde_json::json!({
                        "id": ev.id,
                        "seq": ev.seq,
                        "ts": ev.ts,
                        "kind": ev.kind,
                        "data": serde_json::from_str::<serde_json::Value>(&ev.data).unwrap_or_default(),
                    }),
                });
            }
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("Failed to dispatch message: {}", e)
                })),
            ));
        }
    };

    let status_str = match outcome {
        crate::provider::manager::SendOutcome::Started => "started",
        crate::provider::manager::SendOutcome::Queued => "queued",
    };

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({
        "status": status_str,
        "session_id": id,
    })))
}

/// POST /api/sessions/:id/cancel -- kill the running process, append agent-end
/// with crashed/operator-stop and broadcast it. Returns 204.
pub(super) async fn cancel_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(session_id = %id, "Cancelling session");
    let session = state.db.get_session(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    if session.is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        ));
    }

    // Discard any queued follow-up first: this is an explicit stop, so the
    // completion listener must not drain the queue and respawn a fresh run.
    crate::provider::manager::clear_queued_message(&state.db, &state.broadcaster, &id).await;

    // Kill the running process (if any)
    state.session_manager.cancel(&id).await;

    let event = state
        .db
        .append_event(
            &id,
            "agent-end",
            serde_json::json!({ "status": "crashed", "reason": "operator-stop" }),
        )
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    state
        .broadcaster
        .broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "event".into(),
            session_id: id,
            data: serde_json::json!({
                "id": event.id,
                "seq": event.seq,
                "ts": event.ts,
                "kind": event.kind,
                "data": serde_json::from_str::<serde_json::Value>(&event.data).unwrap_or_default(),
            }),
        });

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(StatusCode::NO_CONTENT)
}

/// POST /api/sessions/:id/interrupt -- interrupt the running process,
/// append an interrupt event with user-interrupt reason, and broadcast it.
/// Returns 204.
pub(super) async fn interrupt_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(session_id = %id, "Interrupting session");
    let session = state.db.get_session(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    if session.is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        ));
    }

    // NOTE: interrupt deliberately does NOT clear the queued message. It is
    // the "release the current turn so my queued follow-up runs" affordance;
    // the completion listener drains the queue afterwards by design (see
    // `drain_queued_delivers_after_interrupted_run` and the session-lifecycle
    // e2e). A hard stop that discards the queue is `/cancel` or `/terminate`.

    // Interrupt the running process (if any)
    state.session_manager.interrupt(&id).await;

    let event = state
        .db
        .append_event(
            &id,
            "interrupt",
            serde_json::json!({ "reason": "user-interrupt" }),
        )
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    state
        .broadcaster
        .broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "event".into(),
            session_id: id,
            data: serde_json::json!({
                "id": event.id,
                "seq": event.seq,
                "ts": event.ts,
                "kind": event.kind,
                "data": serde_json::from_str::<serde_json::Value>(&event.data).unwrap_or_default(),
            }),
        });

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(StatusCode::NO_CONTENT)
}

/// POST /api/sessions/:id/terminate -- kill the long-lived agent process so
/// the next message spawns a fresh one (picking up new skills, MCP config,
/// etc). Distinct from /cancel and /interrupt: those exist to stop an active
/// turn and append a `agent-end{crashed}` or `interrupt` event that pairs
/// with an in-flight `agent-start`. Terminate is meant to be used between
/// turns when the child is idle but still alive, so it emits a neutral
/// `system` notice rather than a lifecycle event that would be misleading
/// without a matching `agent-start`. Returns 204.
pub(super) async fn terminate_agent(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(session_id = %id, "Terminating agent process");
    let session = state.db.get_session(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    if session.is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        ));
    }

    // Discard any queued follow-up first: terminate means "fresh start on
    // the next message", so the completion listener must not drain the queue
    // and respawn a run the moment this cancel's completion fires.
    crate::provider::manager::clear_queued_message(&state.db, &state.broadcaster, &id).await;

    // Wait for the provider's stream loop to wind down (and emit its own
    // synthetic Crashed event if a turn was active) before appending the
    // terminate notice, so the transcript order is process-end → notice.
    state.session_manager.cancel_and_wait(&id).await;

    let event = state
        .db
        .append_event(
            &id,
            "system",
            serde_json::json!({
                "text": "Agent terminated. The next message will start a fresh process.",
            }),
        )
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    state
        .broadcaster
        .broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "event".into(),
            session_id: id,
            data: serde_json::json!({
                "id": event.id,
                "seq": event.seq,
                "ts": event.ts,
                "kind": event.kind,
                "data": serde_json::from_str::<serde_json::Value>(&event.data).unwrap_or_default(),
            }),
        });

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(StatusCode::NO_CONTENT)
}

/// POST /api/sessions/:id/prehatch-cancel -- cancel the pre-hatch parked on
/// this chat session: kill the temp research session's agent, dismiss the
/// pending question cards, and make sure the parked original message still
/// reaches the main model. Delivery is routed through the plugin that owns
/// the pre-hatch (`session.prehatch.cancel`) so its pending records are
/// cleared with it; when no plugin handles the hook (uninstalled or disabled
/// since the pre-hatch started) core delivers the original itself —
/// cancelling must never eat the user's message.
pub(super) async fn cancel_pre_hatch(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Extension(user): Extension<AuthUser>,
) -> impl IntoResponse {
    tracing::info!(session_id = %id, "Cancelling pre-hatch");
    let session = state
        .db
        .get_session(&id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "session not found" })),
            )
        })?;

    // The pre-hatch in flight is the newest placeholder with no `user` event
    // after it — delivery (or the user typing past a dead pre-hatch)
    // supersedes the placeholder; same rule the chat UI renders by.
    // `pre-ignite` is the legacy kind from before the pre-hatcher rename.
    let events = state
        .db
        .list_events_by_session_before(&id, None, 200)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;
    let parked = events
        .iter()
        .rev()
        .find(|e| matches!(e.kind.as_str(), "user" | "pre-hatch" | "pre-ignite"))
        .filter(|e| e.kind != "user");
    let Some(parked) = parked else {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "no pre-hatch in flight" })),
        ));
    };
    let parked_seq = parked.seq;
    let parked_data: serde_json::Value = serde_json::from_str(&parked.data).unwrap_or_default();
    let text = parked_data
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let temp_session_id = parked_data
        .get("temp_session_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if text.is_empty() {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "pre-hatch event carries no text" })),
        ));
    }

    // Kill the research agent FIRST so it can't race this cancel with a
    // delivery of its own, then note the stop on its transcript.
    if let Some(temp_id) = &temp_session_id {
        crate::provider::manager::clear_queued_message(&state.db, &state.broadcaster, temp_id)
            .await;
        state.session_manager.cancel_and_wait(temp_id).await;
        append_and_broadcast(
            &state,
            temp_id,
            "system",
            serde_json::json!({
                "text": "Pre-hatch cancelled by the user; research agent terminated.",
            }),
        )
        .await;
    }

    // The temp agent may have delivered in the window before the kill; a
    // `user` event after the placeholder means there is nothing to cancel.
    let delivered = state
        .db
        .list_events_by_session(&id, Some(parked_seq))
        .await
        .map(|evs| evs.iter().any(|e| e.kind == "user"))
        .unwrap_or(false);
    if delivered {
        return Ok(Json(serde_json::json!({
            "status": "already-delivered",
            "session_id": id,
        })));
    }

    // Any question card still up (opt-in, clarifying, approval) belongs to
    // the pre-hatch being cancelled; its redirect target is now dead.
    dismiss_pending_questions(&state, &id).await;

    // A `Cancelled` verdict means the plugin delivered the original (or knew
    // it was already delivered); anything else falls through to core's own
    // delivery below.
    let hook = crate::plugin::hooks::PREHATCH_CANCEL_HOOK;
    let plugin_handled = state.plugins.has_listeners(hook).await
        && state
            .plugins
            .dispatch_scoped(
                hook,
                &user.user_id,
                Some(session.folder_id.clone()),
                session.project_id.clone(),
                Some(id.clone()),
                serde_json::json!({
                    "session_id": id,
                    "temp_session_id": temp_session_id,
                    "text": text,
                }),
            )
            .await
            .is_cancelled();

    if !plugin_handled {
        let mut pre_hatch = serde_json::json!({
            "original": text,
            "enriched": false,
            "cancelled": true,
        });
        if let Some(temp_id) = &temp_session_id {
            pre_hatch["temp_session_id"] = serde_json::json!(temp_id);
        }
        append_and_broadcast(
            &state,
            &id,
            "user",
            serde_json::json!({ "text": text, "pre_hatch": pre_hatch }),
        )
        .await;
        // Resume exactly like the plugin's deliver path: run the main model
        // on the original text without appending a second user event.
        use crate::service::mcp_server::ExpertDispatcher;
        if let Err(e) = crate::service::mcp_server::AppExpertDispatcher::new(state.clone())
            .resume_session(&id, &text)
            .await
        {
            tracing::warn!(session_id = %id, "pre-hatch cancel resume failed: {e}");
        }
    }

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({
        "status": "cancelled",
        "session_id": id,
        "delivered_by": if plugin_handled { "plugin" } else { "core" },
    })))
}

/// Append an event and broadcast it — the persist+push pair the cancel path
/// repeats. Failures log and drop: every caller is past the point where an
/// event write should fail the user's action.
async fn append_and_broadcast(
    state: &Arc<AppState>,
    session_id: &str,
    kind: &str,
    data: serde_json::Value,
) {
    match state.db.append_event(session_id, kind, data).await {
        Ok(ev) => {
            state
                .broadcaster
                .broadcast(crate::ws::broadcaster::WsEvent {
                    event_type: "event".into(),
                    session_id: session_id.to_string(),
                    data: serde_json::json!({
                        "id": ev.id,
                        "seq": ev.seq,
                        "ts": ev.ts,
                        "kind": ev.kind,
                        "data": serde_json::from_str::<serde_json::Value>(&ev.data)
                            .unwrap_or_default(),
                    }),
                });
        }
        Err(e) => {
            tracing::warn!(session_id = %session_id, kind = %kind, "Failed to append event: {e}");
        }
    }
}

/// Read every attachment referenced by the request into memory so the
/// provider can build a multimodal envelope without a second round-trip
/// through the data dir. A missing id drops with a warning — at
/// dispatch time we want best-effort delivery of the rest of the
/// message rather than a hard failure that the user can't recover
/// from (the id might be stale because the FE held on to a soft-
/// deleted upload, etc.).
async fn load_attachments(
    session_id: &str,
    ids: Option<&[String]>,
    state: &Arc<AppState>,
) -> Vec<UserAttachment> {
    let Some(ids) = ids else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(ids.len());
    for aid in ids {
        match load_attachment_payload(&state.config.data_dir, session_id, aid).await {
            Some(payload) => out.push(UserAttachment {
                filename: payload.filename,
                mime_type: payload.mime_type,
                data: payload.data,
            }),
            None => tracing::warn!(
                session_id = %session_id,
                attachment_id = %aid,
                "Skipping unknown attachment in send_message"
            ),
        }
    }
    out
}

/// Resolve every outstanding `question` event for the session by appending a
/// `question-resolved {rejected: true}` for each id that doesn't already have
/// one. Persists + broadcasts directly (no `/events` route), so we don't
/// trigger the route's question-resolved → agent-respawn side effect — the
/// caller is about to dispatch the actual user message and start a turn on
/// its own.
async fn dismiss_pending_questions(state: &Arc<AppState>, session_id: &str) {
    let events = match state.db.list_events_by_session(session_id, None).await {
        Ok(events) => events,
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                "Failed to scan events for pending questions: {e}"
            );
            return;
        }
    };

    let mut resolved: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut question_ids: Vec<String> = Vec::new();
    for ev in &events {
        match ev.kind.as_str() {
            "question" => question_ids.push(ev.id.clone()),
            "question-resolved" => {
                if let Ok(data) = serde_json::from_str::<serde_json::Value>(&ev.data) {
                    let qid = data
                        .get("question_id")
                        .or_else(|| data.get("questionId"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    if let Some(qid) = qid {
                        resolved.insert(qid);
                    }
                }
            }
            _ => {}
        }
    }

    for qid in question_ids {
        if resolved.contains(&qid) {
            continue;
        }
        let data = serde_json::json!({
            "question_id": qid,
            "rejected": true,
            "reason": "superseded-by-user-message",
        });
        match state
            .db
            .append_event(session_id, "question-resolved", data.clone())
            .await
        {
            Ok(ev) => {
                state.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
                    event_type: "event".into(),
                    session_id: session_id.to_string(),
                    data: serde_json::json!({
                        "id": ev.id,
                        "seq": ev.seq,
                        "ts": ev.ts,
                        "kind": ev.kind,
                        "data": serde_json::from_str::<serde_json::Value>(&ev.data).unwrap_or_default(),
                    }),
                });
            }
            Err(e) => tracing::warn!(
                session_id = %session_id,
                question_id = %qid,
                "Failed to auto-dismiss pending question: {e}"
            ),
        }
    }
}

/// GET /api/sessions/:id/status -- derive agent status from the event tail.
pub(super) async fn get_session_status(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(session_id = %id, "Getting session status");
    let session = state.db.get_session(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    if session.is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        ));
    }

    let tail = state.db.events_tail(&id, 10).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    let status = derive_status(&tail);

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({ "status": status })))
}

/// Walk the event tail to derive the current agent status.
fn derive_status(events: &[crate::db::models::Event]) -> &'static str {
    // Track the latest lifecycle positions
    let mut last_agent_start: Option<usize> = None;
    let mut last_agent_end: Option<usize> = None;
    let mut last_tool_start: Option<usize> = None;
    let mut last_tool_end: Option<usize> = None;
    let mut last_question: Option<usize> = None;
    let mut last_question_resolved: Option<usize> = None;

    for (i, event) in events.iter().enumerate() {
        match event.kind.as_str() {
            "agent-start" => last_agent_start = Some(i),
            "agent-end" => last_agent_end = Some(i),
            "agent-tool-start" => last_tool_start = Some(i),
            "agent-tool-end" => last_tool_end = Some(i),
            "question" => last_question = Some(i),
            "question-resolved" => last_question_resolved = Some(i),
            _ => {}
        }
    }

    // Check if latest agent-end has status "crashed"
    if let Some(end_idx) = last_agent_end {
        // Only consider it if there is no agent-start after this end
        let agent_ended = last_agent_start.map_or(true, |s| s < end_idx);
        if agent_ended {
            if let Ok(data) = serde_json::from_str::<serde_json::Value>(&events[end_idx].data) {
                if data.get("status").and_then(|v| v.as_str()) == Some("crashed") {
                    return "crashed";
                }
            }
        }
    }

    // Check if we are within an active agent run (agent-start with no agent-end after it)
    let agent_active = match (last_agent_start, last_agent_end) {
        (Some(start), Some(end)) => start > end,
        (Some(_), None) => true,
        _ => false,
    };

    if agent_active {
        // Check for unresolved question within the active run
        let has_unresolved_question = match (last_question, last_question_resolved) {
            (Some(q), Some(r)) => q > r,
            (Some(_), None) => true,
            _ => false,
        };

        if has_unresolved_question {
            return "questioning";
        }

        // Check if tool is active (agent-tool-start without agent-tool-end after it)
        let tool_active = match (last_tool_start, last_tool_end) {
            (Some(ts), Some(te)) => ts > te,
            (Some(_), None) => true,
            _ => false,
        };

        if tool_active {
            return "tool-active";
        }

        return "working";
    }

    // Check for unresolved question even outside of active agent run
    let has_unresolved_question = match (last_question, last_question_resolved) {
        (Some(q), Some(r)) => q > r,
        (Some(_), None) => true,
        _ => false,
    };

    if has_unresolved_question {
        return "questioning";
    }

    "idle"
}
