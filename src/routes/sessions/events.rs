use axum::{
    Json, extract::Path, extract::Query, extract::State, http::StatusCode, response::IntoResponse,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::db::models::UpdateSession;
use crate::provider::stream::SpawnConfig;
use crate::state::AppState;

use super::resolve_references;

#[derive(Deserialize)]
pub(super) struct EventsQuery {
    after_seq: Option<i32>,
    limit: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct AppendEventRequest {
    kind: String,
    data: serde_json::Value,
}

/// GET /api/sessions/:id/events -- list events with optional afterSeq + limit
pub(super) async fn list_events(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<EventsQuery>,
) -> impl IntoResponse {
    tracing::info!(session_id = %id, after_seq = ?params.after_seq, "Listing events");
    let events = if let Some(after_seq) = params.after_seq {
        state.db.events_since(&id, after_seq).await
    } else {
        state.db.list_events_by_session(&id, None).await
    };

    let mut events = events.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    // Apply limit if specified
    if let Some(limit) = params.limit {
        events.truncate(limit as usize);
    }

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

        // Resume the conversation under the per-session lock, mirroring
        // the /message route's atomic check-and-act: if an agent is
        // already running (e.g. the user's answer raced an orchestrator
        // respawn or a parallel send), queue instead of spawning a second
        // run. Spawned so the HTTP response returns immediately.
        let state_clone = state.clone();
        let id_clone = id.clone();
        tokio::spawn(async move {
            let lock = state_clone.session_manager.lock_session(&id_clone).await;

            if state_clone.session_manager.is_running(&id_clone).await {
                // Persist the answer for the in-flight run's completion
                // listener to drain. We deliberately do NOT append a user
                // event here — drain_queued appends one before dispatching,
                // matching the ordering in the conversation log.
                let now = chrono::Utc::now().to_rfc3339();
                if let Err(e) = state_clone
                    .db
                    .upsert_queued_message(crate::db::models::NewQueuedMessage {
                        session_id: id_clone.clone(),
                        text: answer_text.clone(),
                        queued_at: now,
                        model: None,
                        effort: None,
                    })
                    .await
                {
                    tracing::error!(
                        session_id = %id_clone,
                        "Failed to queue question answer: {e}"
                    );
                    return;
                }
                state_clone
                    .broadcaster
                    .broadcast(crate::ws::broadcaster::WsEvent {
                        event_type: "queue".into(),
                        session_id: id_clone.clone(),
                        data: serde_json::json!({ "action": "set", "text": answer_text }),
                    });
                tracing::info!(
                    session_id = %id_clone,
                    "Question answer queued; will deliver on next completion"
                );
                return;
            }

            // Not running — append the user event, then dispatch.
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
            };

            if let Err(e) = state_clone
                .session_manager
                .send_message_locked(
                    &lock,
                    &answer_text,
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
/// Todos are derived from the event log (the latest `todo` event wins), not a
/// dedicated table. Returns `{ "todos": [...] }`, an empty list when the
/// session has never reported any. This is the load-time read path; live
/// updates still arrive over the WebSocket as `todo` events.
pub(super) async fn get_session_todos(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(session_id = %id, "Getting session todos");
    let latest = state
        .db
        .latest_event_of_kind(&id, "todo")
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    let todos = latest
        .and_then(|e| serde_json::from_str::<serde_json::Value>(&e.data).ok())
        .and_then(|d| d.get("todos").cloned())
        .unwrap_or_else(|| serde_json::json!([]));

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({ "todos": todos })))
}
