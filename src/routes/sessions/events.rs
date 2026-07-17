use axum::{
    Extension, Json, extract::Path, extract::Query, extract::State, http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::auth::middleware::AuthUser;
use crate::db::models::UpdateSession;
use crate::provider::stream::SpawnConfig;
use crate::state::AppState;

use super::resolve_references;

#[derive(Deserialize)]
pub(super) struct EventsQuery {
    /// WS-catch-up mode: return every event with `seq > after_seq`,
    /// oldest-first. Used by the live-update path after a reconnect to
    /// pull anything missed while the socket was down. Returns all
    /// matching rows (no `limit` applied) so the client can be sure it
    /// hasn't lost an event in the gap.
    after_seq: Option<i32>,
    /// Pagination mode: return the latest `limit` events with
    /// `seq < before_seq`, oldest-first. Used by the chat view's
    /// "Load older messages" path. Implies `limit` (capped at
    /// [`MAX_EVENTS_PAGE_SIZE`]).
    before_seq: Option<i32>,
    /// Page size. Applied to `before_seq` and the default "latest N"
    /// mode; ignored for `after_seq` so WS catch-up can't lose events.
    /// Defaults to [`DEFAULT_EVENTS_PAGE_SIZE`], capped at
    /// [`MAX_EVENTS_PAGE_SIZE`].
    limit: Option<i64>,
}

/// Default page size for the chat view's initial fetch. Tuned so the
/// 90% case (recent conversation) loads in one shot without paying the
/// cost of an unbounded event scan, while still showing enough scrollback
/// that the user rarely needs to "Load older".
pub const DEFAULT_EVENTS_PAGE_SIZE: i64 = 200;

/// Hard ceiling on `?limit=` for events. Stops a client from asking for
/// every event in a session (the bug we're fixing) by passing
/// `?limit=99999999`. 1000 is large enough that "Load older" twice
/// covers ~2k events of scrollback before any extra round-trip; past
/// that point the user is digging through history and a few clicks is
/// acceptable.
pub const MAX_EVENTS_PAGE_SIZE: i64 = 1000;

#[derive(Deserialize)]
pub(super) struct AppendEventRequest {
    kind: String,
    data: serde_json::Value,
}

/// GET /api/sessions/:id/events
///
/// Three modes, picked by query param:
/// - **`?after_seq=N`** — WS catch-up. Returns every event with `seq > N`,
///   oldest-first. Returns all matching rows (no limit) so a reconnect
///   never silently drops events.
/// - **`?before_seq=N&limit=K`** — "Load older" page. Returns up to K
///   events with `seq < N`, oldest-first.
/// - **No params (or just `?limit=`)** — Default load. Returns the
///   latest K events (default [`DEFAULT_EVENTS_PAGE_SIZE`]),
///   oldest-first.
///
/// All modes return a flat JSON array. The chat view tracks the lowest
/// `seq` it has and passes it as `before_seq` to page upward.
pub(super) async fn list_events(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<EventsQuery>,
) -> impl IntoResponse {
    tracing::info!(
        session_id = %id,
        after_seq = ?params.after_seq,
        before_seq = ?params.before_seq,
        limit = ?params.limit,
        "Listing events",
    );

    let events = if let Some(after_seq) = params.after_seq {
        // WS catch-up: return all events strictly after `after_seq`.
        // Intentionally NOT limited — losing events in the catch-up
        // path produces silent UI desync.
        state.db.events_since(&id, after_seq).await
    } else {
        let limit = params
            .limit
            .unwrap_or(DEFAULT_EVENTS_PAGE_SIZE)
            .clamp(1, MAX_EVENTS_PAGE_SIZE);
        state
            .db
            .list_events_by_session_before(&id, params.before_seq, limit)
            .await
    };

    let events = events.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    // Parse data field from string to JSON for each event
    let events_json: Vec<serde_json::Value> = events
        .iter()
        .map(|e| {
            serde_json::json!({
                "id": e.id,
                "session_id": e.session_id,
                "seq": e.seq,
                "ts": e.ts,
                "kind": e.kind,
                "data": serde_json::from_str::<serde_json::Value>(&e.data).unwrap_or_default(),
            })
        })
        .collect();

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!(events_json)))
}

/// POST /api/sessions/:id/events -- append an event
pub(super) async fn append_event(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<AppendEventRequest>,
) -> impl IntoResponse {
    tracing::info!(session_id = %id, kind = %body.kind, "Appending event");
    // Verify session exists
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

    let event_kind = body.kind.clone();
    let event_data = body.data.clone();

    let event = state
        .db
        .append_event(&id, &body.kind, body.data)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    // Update last_activity to now
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

    // Broadcast the event to WebSocket subscribers
    state
        .broadcaster
        .broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "event".into(),
            session_id: id.clone(),
            data: serde_json::json!({
                "id": event.id,
                "seq": event.seq,
                "ts": event.ts,
                "kind": event.kind,
                "data": serde_json::from_str::<serde_json::Value>(&event.data).unwrap_or_default(),
            }),
        });

    // If this is a question-resolved event, resume the conversation with
    // the user's answers as a new message. The agent's ask_user MCP tool
    // already completed and the agent turn ended, so we need to start a
    // new turn with the answers.
    if event_kind == "question-resolved" {
        let rejected = event_data
            .get("rejected")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let question_id = event_data
            .get("question_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // When a user answers a worker's question, hand the Q&A to whichever
        // plugin owns question experts (see USER_ANSWER_HOOK). Captured here as
        // (project_id, qa_text) and fired after the conversation resumes; core
        // itself knows nothing about experts.
        let mut question_expert_feed: Option<(String, String)> = None;

        // Build a human-readable answer message to resume the conversation
        let answer_text = if rejected {
            "The user dismissed the question without answering. The questions have been removed from the UI and are no longer visible. Do NOT say the questions are still up. If you still need answers, you must ask again using mcp__peckboard__ask_user.".to_string()
        } else {
            let answers = event_data
                .get("answers")
                .cloned()
                .unwrap_or(serde_json::json!({}));

            // Look up original questions to build readable answer text
            let mut parts = Vec::new();
            if !question_id.is_empty() {
                if let Ok(Some(q_event)) = state.db.get_event(question_id).await {
                    if let Ok(q_data) = serde_json::from_str::<serde_json::Value>(&q_event.data) {
                        if let Some(questions_arr) =
                            q_data.get("questions").and_then(|v| v.as_array())
                        {
                            if let Some(answers_obj) = answers.as_object() {
                                for (idx_str, value) in answers_obj {
                                    if let Ok(idx) = idx_str.parse::<usize>() {
                                        if let Some(q) = questions_arr.get(idx) {
                                            let q_text = q
                                                .get("question")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("Question");
                                            parts.push(format!(
                                                "**{}**: {}",
                                                q_text,
                                                value.as_str().unwrap_or("")
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            let answers_text = if parts.is_empty() {
                format!(
                    "User answered: {}",
                    serde_json::to_string(&answers).unwrap_or_default()
                )
            } else {
                format!(
                    "The user answered your questions (the question form has been removed from the UI):\n\n{}",
                    parts.join("\n")
                )
            };

            // Check if this is a worker session with more pending questions
            let session_info = state.db.get_session(&id).await.ok().flatten();
            let has_more = if let Some(ref sess) = session_info {
                if sess.is_worker {
                    if let Some(ref project_id) = sess.project_id {
                        // Count remaining unresolved questions for this project
                        let worker_sessions = state
                            .db
                            .list_worker_sessions_by_project(project_id)
                            .await
                            .unwrap_or_default();
                        let mut remaining = 0u32;
                        for ws in &worker_sessions {
                            let events = state
                                .db
                                .list_events_by_session(&ws.id, None)
                                .await
                                .unwrap_or_default();
                            let resolved_ids: std::collections::HashSet<String> = events
                                .iter()
                                .filter(|e| e.kind == "question-resolved")
                                .filter_map(|e| {
                                    serde_json::from_str::<serde_json::Value>(&e.data)
                                        .ok()
                                        .and_then(|d| {
                                            d.get("question_id")
                                                .or(d.get("questionId"))
                                                .and_then(|v| v.as_str())
                                                .map(|s| s.to_string())
                                        })
                                })
                                .collect();
                            // Exclude the question we just answered
                            remaining += events
                                .iter()
                                .filter(|e| {
                                    e.kind == "question"
                                        && !resolved_ids.contains(&e.id)
                                        && e.id != question_id
                                })
                                .count() as u32;
                        }
                        remaining > 0
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };

            // A worker question answered by the user: feed the readable Q&A to
            // the project's question expert(s) via the plugin hook below.
            if !parts.is_empty()
                && let Some(ref sess) = session_info
                && sess.is_worker
                && let Some(ref pid) = sess.project_id
            {
                question_expert_feed = Some((pid.clone(), parts.join("\n")));
            }

            if has_more {
                format!(
                    "{}\n\n**Note:** The user is still answering other worker questions. More answers may follow shortly. Continue working with what you have — do not ask the same questions again.",
                    answers_text
                )
            } else {
                answers_text
            }
        };

        // Resolve references in the answer text (e.g. [session:id] from autocomplete)
        let answer_text = resolve_references(&answer_text, &state).await;

        // Notify question-expert plugins of the Q&A, under the answering user's
        // authority. Fire-and-forget: it must not delay the conversation resume,
        // and a plugin failure must not fail the answer.
        if let Some((project_id, qa_text)) = question_expert_feed {
            let plugins = state.plugins.clone();
            let asker_session_id = id.clone();
            let user_id = user.user_id.clone();
            tokio::spawn(async move {
                plugins
                    .dispatch_authed(
                        crate::plugin::hooks::USER_ANSWER_HOOK,
                        &user_id,
                        serde_json::json!({
                            "asker_session_id": asker_session_id,
                            "project_id": project_id,
                            "qa_text": qa_text,
                        }),
                    )
                    .await;
            });
        }

        // Resume the conversation. With the long-lived stream-json
        // process we just append the user event and write the answer
        // to stdin via `send_or_queue` — if the agent is still mid-
        // turn (because another worker reply was streaming back), the
        // CLI buffers the user envelope and consumes it after the
        // current `result`. Spawned so the HTTP response returns
        // immediately.
        // A plugin question may redirect the answer to another session: the
        // pre-hatcher's clarifying question renders on the chat session, but
        // the answer must feed its temp research session — resuming the chat
        // agent with a bare answer would start the very turn the plugin is
        // still preparing. The target is read from the question event core
        // itself persisted (host-side), never from the client request.
        // The question event core persisted carries both the redirect target
        // and the plugin's correlation token (`approval_token`).
        let q_event_data: Option<serde_json::Value> = if question_id.is_empty() {
            None
        } else {
            state
                .db
                .get_event(question_id)
                .await
                .ok()
                .flatten()
                .and_then(|q| serde_json::from_str::<serde_json::Value>(&q.data).ok())
        };
        let redirect_target = q_event_data.as_ref().and_then(|d| {
            d.get("redirectSessionId")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        });
        let approval_token = q_event_data
            .as_ref()
            .and_then(|d| d.get("approval_token").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();
        // The single selected option label (these plugin cards are one
        // question); empty when the user dismissed the card.
        let answer_label = event_data
            .get("answers")
            .and_then(|a| a.as_object())
            .and_then(|o| o.values().next())
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let state_clone = state.clone();
        let id_clone = redirect_target.clone().unwrap_or_else(|| id.clone());
        let chat_id = id.clone();
        let hook_user_id = user.user_id.clone();
        let redirect_for_hook = redirect_target.clone();
        let answer_rejected = rejected;
        tokio::spawn(async move {
            // A pre-hatcher question (opt-in / enriched-message approval) is
            // resolved in the plugin's CODE, not by resuming the cheap model:
            // fire the answer hook and, when the plugin owns the outcome
            // (delivered the message, or dispatched the read-only research
            // turn), skip resuming the temp agent with the raw answer. A
            // non-owning verdict (e.g. a clarifying-question continuation the
            // research agent must read) falls through to the normal resume.
            if let Some(ref temp_id) = redirect_for_hook {
                let is_pre_hatcher = state_clone
                    .db
                    .get_session(temp_id)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|s| s.expert_kind)
                    .as_deref()
                    == Some(crate::service::mcp_server::PRE_HATCHER_EXPERT_KIND);
                if is_pre_hatcher {
                    let chat_sess = state_clone.db.get_session(&chat_id).await.ok().flatten();
                    let folder = chat_sess.as_ref().map(|s| s.folder_id.clone());
                    let project = chat_sess.as_ref().and_then(|s| s.project_id.clone());
                    let res = state_clone
                        .plugins
                        .dispatch_scoped(
                            crate::plugin::hooks::PREHATCH_ANSWER_HOOK,
                            &hook_user_id,
                            folder,
                            project,
                            Some(chat_id.clone()),
                            serde_json::json!({
                                "chat_session_id": chat_id,
                                "temp_session_id": temp_id,
                                "token": approval_token,
                                "answer": answer_label,
                                "rejected": answer_rejected,
                            }),
                        )
                        .await;
                    if res.is_cancelled() {
                        return;
                    }
                }
            }
            // Append the user event up front so the conversation log
            // reflects the typed order regardless of mid-turn vs. idle.
            if let Ok(user_ev) = state_clone
                .db
                .append_event(&id_clone, "user", serde_json::json!({"text": &answer_text}))
                .await
            {
                state_clone
                    .broadcaster
                    .broadcast(crate::ws::broadcaster::WsEvent {
                        event_type: "event".into(),
                        session_id: id_clone.clone(),
                        data: serde_json::json!({
                            "id": user_ev.id,
                            "seq": user_ev.seq,
                            "ts": user_ev.ts,
                            "kind": "user",
                            "data": {"text": &answer_text},
                        }),
                    });
            }

            let session_project_id = state_clone
                .db
                .get_session(&id_clone)
                .await
                .ok()
                .flatten()
                .and_then(|s| s.project_id);
            let mcp_token = state_clone
                .mcp_tokens
                .issue_token(id_clone.clone(), session_project_id)
                .await;
            let mcp_config_path = crate::service::mcp_server::write_mcp_config(
                &state_clone.config.data_dir,
                &id_clone,
                state_clone.config.port,
                &mcp_token,
            )
            .ok()
            .map(|p| p.to_string_lossy().to_string());

            let config = SpawnConfig {
                model: "default".into(),
                effort: None,
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

            if let Err(e) = state_clone
                .session_manager
                .send_or_queue(
                    &id_clone,
                    crate::provider::message::UserMessage::from_text(answer_text),
                    &state_clone.db,
                    &state_clone.broadcaster,
                    config,
                )
                .await
            {
                tracing::error!(session_id = %id_clone, "Failed to resume session with answer: {e}");
            }
        });
    }

    Ok::<_, (StatusCode, Json<serde_json::Value>)>((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "id": event.id,
            "seq": event.seq,
            "ts": event.ts,
            "kind": event.kind,
        })),
    ))
}

/// GET /api/sessions/:id/todos -- current todo snapshot for the session.
///
/// Reads from the dedicated `todos` table, which the `todo` event seam
/// mirrors into on every replace-all snapshot. Returns
/// `{ "todos": [...] }` in `position` order; empty list when the session
/// has never reported any. Live updates still flow over the WebSocket as
/// `todo` events.
pub(super) async fn get_session_todos(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(session_id = %id, "Getting session todos");
    let todos = state.db.list_session_todos(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({ "todos": todos })))
}
