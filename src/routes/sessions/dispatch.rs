use axum::{Json, extract::Path, extract::State, http::StatusCode, response::IntoResponse};
use serde::Deserialize;
use std::sync::Arc;

use crate::db::models::UpdateSession;
use crate::provider::stream::SpawnConfig;
use crate::state::AppState;

use super::resolve_references;

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

    let attachment_ids = body.attachment_ids.clone();

    // Resolve [session:id] and [report:folder/file] references early; both
    // the queued and started paths use the resolved text.
    let resolved_text = resolve_references(&body.text, &state).await;

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
    };

    // `send_or_queue` acquires the per-session lock internally,
    // dispatches through the long-lived child (spawning lazily on
    // the first turn) and returns `Queued` iff the agent was
    // already mid-turn when the bytes hit stdin.
    let outcome = state
        .session_manager
        .send_or_queue(&id, &resolved_text, &state.db, &state.broadcaster, config)
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
