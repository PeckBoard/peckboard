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

    // Atomic check-and-act under the per-session lock: if an agent is
    // already running, queue the message; otherwise spawn a fresh run.
    // Holding the lock across the is_running check + action prevents two
    // concurrent POSTs from both spawning agents on the same session.
    let lock = state.session_manager.lock_session(&id).await;

    if state.session_manager.is_running(&id).await {
        if attachment_ids.as_ref().is_some_and(|a| !a.is_empty()) {
            tracing::warn!(
                session_id = %id,
                "Attachments dropped on queued message (queue does not persist them)"
            );
        }

        let now = chrono::Utc::now().to_rfc3339();
        let queued = state
            .db
            .upsert_queued_message(crate::db::models::NewQueuedMessage {
                session_id: id.clone(),
                text: resolved_text.clone(),
                queued_at: now,
                model: Some(resolved_model.clone()),
                effort: resolved_effort.clone(),
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
                event_type: "queue".into(),
                session_id: id.clone(),
                data: serde_json::json!({ "action": "set", "text": queued.text }),
            });

        return Ok(Json(serde_json::json!({
            "status": "queued",
            "session_id": id,
        })));
    }

    // Not running — append the user event, then dispatch a fresh run.
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

    if let Err(e) = state
        .session_manager
        .send_message_locked(&lock, &resolved_text, &state.db, &state.broadcaster, config)
        .await
    {
        tracing::error!(session_id = %id, "Failed to spawn claude process: {}", e);

        let crash_event = state
            .db
            .append_event(
                &id,
                "agent-end",
                serde_json::json!({
                    "status": "crashed",
                    "reason": format!("spawn error: {}", e),
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
            Json(serde_json::json!({ "error": format!("Failed to spawn agent: {}", e) })),
        ));
    }

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({
        "status": "started",
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
