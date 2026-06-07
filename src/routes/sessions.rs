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
                format!("The user answered your questions (the question form has been removed from the UI):\n\n{}", parts.join("\n"))
            };

            // Check if this is a worker session with more pending questions
            let session_info = state.db.get_session(&id).await.ok().flatten();
            let has_more = if let Some(ref sess) = session_info {
                if sess.is_worker {
                    if let Some(ref project_id) = sess.project_id {
                        // Count remaining unresolved questions for this project
                        let worker_sessions = state.db.list_worker_sessions_by_project(project_id).await.unwrap_or_default();
                        let mut remaining = 0u32;
                        for ws in &worker_sessions {
                            let events = state.db.list_events_by_session(&ws.id, None).await.unwrap_or_default();
                            let resolved_ids: std::collections::HashSet<String> = events.iter()
                                .filter(|e| e.kind == "question-resolved")
                                .filter_map(|e| serde_json::from_str::<serde_json::Value>(&e.data).ok()
                                    .and_then(|d| d.get("question_id").or(d.get("questionId")).and_then(|v| v.as_str()).map(|s| s.to_string())))
                                .collect();
                            // Exclude the question we just answered
                            remaining += events.iter()
                                .filter(|e| e.kind == "question" && !resolved_ids.contains(&e.id) && e.id != question_id)
                                .count() as u32;
                        }
                        remaining > 0
                    } else { false }
                } else { false }
            } else { false };

            if has_more {
                format!("{}\n\n**Note:** The user is still answering other worker questions. More answers may follow shortly. Continue working with what you have — do not ask the same questions again.", answers_text)
            } else {
                answers_text
            }
        };

        // Send as a new message to resume the conversation
        let state_clone = state.clone();
        let id_clone = id.clone();
        tokio::spawn(async move {
            // Append a user event for the answer
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

            // Issue MCP token with project scope (for worker sessions)
            let session_project_id = state_clone.db.get_session(&id_clone).await
                .ok().flatten().and_then(|s| s.project_id);
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
                .send_message(
                    &id_clone,
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

    // 1. Append a user event with the message text
    let mut user_data = serde_json::json!({ "text": body.text });
    if let Some(ref attachment_ids) = body.attachment_ids {
        user_data["attachmentIds"] = serde_json::json!(attachment_ids);
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

    // Broadcast user event
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

    // Update last_activity
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

    // 2. Build spawn config — resolve model/effort with precedence:
    //    request body > session > card > project > "default"
    let (resolved_model, resolved_effort) = {
        let mut model: Option<String> = body.model;
        let mut effort: Option<String> = body.effort;

        // Fallback to session-level
        if model.is_none() {
            model = session.model.clone();
        }
        if effort.is_none() {
            effort = session.effort.clone();
        }

        // Fallback to card-level, and then project-level
        if model.is_none() || effort.is_none() {
            if let Some(ref card_id) = session.card_id {
                if let Ok(Some(card)) = state.db.get_card(card_id).await {
                    if model.is_none() {
                        model = card.model.clone();
                    }
                    if effort.is_none() {
                        effort = card.effort.clone();
                    }

                    // Fallback to project-level
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

    // Issue MCP token and write config so the session has access to MCP tools
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
        working_dir: String::new(), // Will be resolved by SessionManager from the folder
        mcp_config_path,
        env: Default::default(),
        // Use bypass mode — questions are handled through the peckboard MCP
        // ask_user tool, not the CLI's built-in AskUserQuestion.
        permission_mode: Some("bypass".into()),
        timeout_ms: None,
        metadata: serde_json::Value::Null,
    };

    // 3. Spawn the Claude CLI process in the background
    if let Err(e) = state
        .session_manager
        .send_message(&id, &body.text, &state.db, &state.broadcaster, config)
        .await
    {
        tracing::error!(session_id = %id, "Failed to spawn claude process: {}", e);

        // Append a crashed event so the UI knows it failed
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

    // 4. Return 200 immediately — streaming happens in background
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
