use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{get, post},
};
use serde::Deserialize;
use std::sync::Arc;

use crate::auth::middleware::require_auth;
use crate::db::models::{NewSession, UpdateSession};
use crate::provider::stream::SpawnConfig;
use crate::state::AppState;

#[derive(Deserialize)]
struct CreateSessionRequest {
    name: String,
    folder_id: String,
    model: Option<String>,
    effort: Option<String>,
}

#[derive(Deserialize)]
struct ListSessionsQuery {
    folder_id: Option<String>,
}

#[derive(Deserialize)]
struct UpdateSessionRequest {
    name: Option<String>,
    model: Option<Option<String>>,
    effort: Option<Option<String>>,
    project_id: Option<Option<String>>,
    card_id: Option<Option<String>>,
    conversation_id: Option<Option<String>>,
    last_activity: Option<String>,
}

#[derive(Deserialize)]
struct SendMessageRequest {
    text: String,
    #[serde(default, rename = "attachmentIds")]
    attachment_ids: Option<Vec<String>>,
    model: Option<String>,
    effort: Option<String>,
}

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/sessions", post(create_session).get(list_sessions))
        .route(
            "/api/sessions/{id}",
            get(get_session)
                .patch(update_session)
                .delete(delete_session),
        )
        .route(
            "/api/sessions/{id}/events",
            get(list_events).post(append_event),
        )
        .route("/api/sessions/{id}/read", post(mark_read))
        .route("/api/sessions/{id}/clear", post(clear_session))
        .route("/api/sessions/{id}/message", post(send_message))
        .route("/api/sessions/{id}/cancel", post(cancel_session))
        .route("/api/sessions/{id}/interrupt", post(interrupt_session))
        .route("/api/sessions/{id}/status", get(get_session_status))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

#[derive(Deserialize)]
struct EventsQuery {
    after_seq: Option<i32>,
    limit: Option<i64>,
}

#[derive(Deserialize)]
struct AppendEventRequest {
    kind: String,
    data: serde_json::Value,
}

/// POST /api/sessions
async fn create_session(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateSessionRequest>,
) -> impl IntoResponse {
    tracing::info!(name = %body.name, folder_id = %body.folder_id, "Creating session");
    let now = chrono::Utc::now().to_rfc3339();
    let id = uuid::Uuid::new_v4().to_string();

    let session = state
        .db
        .create_session(NewSession {
            id,
            name: body.name,
            folder_id: body.folder_id,
            model: body.model,
            effort: body.effort,
            is_worker: false,
            project_id: None,
            card_id: None,
            conversation_id: None,
            created_at: now.clone(),
            last_activity: now,
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>((
        StatusCode::CREATED,
        Json(serde_json::json!(session)),
    ))
}

/// GET /api/sessions
async fn list_sessions(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListSessionsQuery>,
) -> impl IntoResponse {
    tracing::info!(folder_id = ?params.folder_id, "Listing sessions");
    let sessions = if let Some(folder_id) = params.folder_id {
        state.db.list_plain_sessions_by_folder(&folder_id).await
    } else {
        state.db.list_plain_sessions().await
    };

    let sessions = sessions.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!(sessions)))
}

/// GET /api/sessions/:id
async fn get_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(session_id = %id, "Getting session");
    let session = state.db.get_session(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    match session {
        Some(s) => Ok(Json(serde_json::json!(s))),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        )),
    }
}

/// PATCH /api/sessions/:id
async fn update_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<UpdateSessionRequest>,
) -> impl IntoResponse {
    tracing::info!(session_id = %id, "Updating session");
    let update = UpdateSession {
        name: body.name,
        model: body.model,
        effort: body.effort,
        project_id: body.project_id,
        card_id: body.card_id,
        conversation_id: body.conversation_id,
        last_activity: body.last_activity,
    };

    let session = state.db.update_session(&id, update).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    match session {
        Some(s) => Ok(Json(serde_json::json!(s))),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        )),
    }
}

/// DELETE /api/sessions/:id
async fn delete_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(session_id = %id, "Deleting session");
    // Delete associated events first
    state.db.delete_events_by_session(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    // Remove attachments directory for this session
    let attachments_dir = state.config.data_dir.join("attachments").join(&id);
    if attachments_dir.exists() {
        let _ = std::fs::remove_dir_all(&attachments_dir);
    }

    // Clean up MCP config and tokens
    crate::service::mcp_server::delete_mcp_config(&state.config.data_dir, &id);
    state.mcp_tokens.revoke_by_session(&id).await;

    let deleted = state.db.delete_session(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    if !deleted {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        ));
    }

    Ok(StatusCode::NO_CONTENT)
}

/// GET /api/sessions/:id/events -- list events with optional afterSeq + limit
async fn list_events(
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
async fn append_event(
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

/// POST /api/sessions/:id/read -- mark session as read
async fn mark_read(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(session_id = %id, "Marking session as read");
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

    state
        .db
        .append_event(&id, "session-read", serde_json::json!({}))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(StatusCode::NO_CONTENT)
}

/// POST /api/sessions/:id/clear -- kill process (placeholder), delete events,
/// delete attachments directory, reset conversation_id. Returns 204.
async fn clear_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(session_id = %id, "Clearing session");
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

    // Kill any running process for this session
    state.session_manager.cancel(&id).await;

    // Delete all events for this session
    state.db.delete_events_by_session(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    // Delete attachments directory
    let attachments_dir = state.config.data_dir.join("attachments").join(&id);
    if attachments_dir.exists() {
        let _ = tokio::fs::remove_dir_all(&attachments_dir).await;
    }

    // Reset conversation_id to None
    state
        .db
        .update_session(
            &id,
            UpdateSession {
                conversation_id: Some(None),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(StatusCode::NO_CONTENT)
}

/// POST /api/sessions/:id/message -- send a message to spawn a Claude CLI process.
/// Appends a user event, spawns the CLI in the background (which emits its own
/// agent-start event via the stream parser), and returns 200 immediately.
async fn send_message(
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
async fn cancel_session(
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
async fn interrupt_session(
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
async fn get_session_status(
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

/// Resolve `[session:id]` and `[report:folder/file]` references in text.
async fn resolve_references(text: &str, state: &Arc<AppState>) -> String {
    let mut result = text.to_string();

    // Resolve session references
    let session_re = regex::Regex::new(r"\[session:([a-f0-9\-]+)\]").unwrap();
    let mut session_replacements = Vec::new();
    for cap in session_re.captures_iter(&result) {
        let full_match = cap[0].to_string();
        let ref_session_id = cap[1].to_string();

        // Hook: session.reference.resolve
        let hook_result = state
            .plugins
            .dispatch(
                "session.reference.resolve",
                serde_json::json!({
                    "referencedSessionId": &ref_session_id,
                }),
            )
            .await;

        if let crate::plugin::hooks::HookResult::Allowed(modified) = &hook_result {
            if let Some(custom) = modified.get("replacement").and_then(|v| v.as_str()) {
                session_replacements.push((full_match, custom.to_string()));
                continue;
            }
        }

        if let Ok(Some(ref_session)) = state.db.get_session(&ref_session_id).await {
            let session_name = &ref_session.name;
            let conv_id = ref_session.conversation_id.as_deref().unwrap_or("unknown");
            let card_info = if let Some(ref card_id) = ref_session.card_id {
                state
                    .db
                    .get_card(card_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|c| format!(" (card: \"{}\")", c.title))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let replacement = format!(
                "[Referenced session \"{}\"{} — conversation_id: {}. \
                 To read this session's full history, you can resume it with \
                 conversation_id \"{}\". The session contains the work and \
                 context from that conversation.]",
                session_name, card_info, conv_id, conv_id
            );
            session_replacements.push((full_match, replacement));
        } else {
            session_replacements.push((
                full_match,
                format!("[session {} not found]", ref_session_id),
            ));
        }
    }
    for (from, to) in session_replacements {
        result = result.replace(&from, &to);
    }

    // Resolve report references
    let report_re = regex::Regex::new(r"\[report:([^\]]+)\]").unwrap();
    let mut report_replacements = Vec::new();
    for cap in report_re.captures_iter(&result) {
        let full_match = cap[0].to_string();
        let report_path = cap[1].to_string();
        let parts: Vec<&str> = report_path.splitn(2, '/').collect();
        if parts.len() == 2 {
            let folder = parts[0];
            let file = parts[1];
            let report_file = state
                .config
                .data_dir
                .join("reports")
                .join(folder)
                .join(file);
            if let Ok(content) = tokio::fs::read_to_string(&report_file).await {
                let body = if content.starts_with("---") {
                    content.splitn(3, "---").nth(2).unwrap_or(&content).trim()
                } else {
                    content.trim()
                };
                let truncated = if body.len() > 2000 {
                    format!("{}... (truncated)", &body[..2000])
                } else {
                    body.to_string()
                };
                report_replacements.push((
                    full_match,
                    format!(
                        "[Report: {}/{}]\n{}\n[End of report]",
                        folder, file, truncated
                    ),
                ));
            } else {
                report_replacements.push((
                    full_match,
                    format!("[report {}/{} not found]", folder, file),
                ));
            }
        }
    }
    for (from, to) in report_replacements {
        result = result.replace(&from, &to);
    }

    result
}
