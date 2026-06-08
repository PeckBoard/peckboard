//! Session HTTP routes. This module holds the simple lifecycle handlers
//! (create/get/list/update/delete/clear/mark_read) plus the
//! `[session:id]` / `[report:folder/file]` reference resolver used by
//! both events and dispatch. Event ingest lives in [`events`] and the
//! send_message + lifecycle controls live in [`dispatch`].

mod dispatch;
mod events;

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
            get(events::list_events).post(events::append_event),
        )
        .route("/api/sessions/{id}/todos", get(events::get_session_todos))
        .route("/api/sessions/{id}/read", post(mark_read))
        .route("/api/sessions/{id}/clear", post(clear_session))
        .route("/api/sessions/{id}/message", post(dispatch::send_message))
        .route("/api/sessions/{id}/cancel", post(dispatch::cancel_session))
        .route(
            "/api/sessions/{id}/interrupt",
            post(dispatch::interrupt_session),
        )
        .route(
            "/api/sessions/{id}/status",
            get(dispatch::get_session_status),
        )
        .route_layer(middleware::from_fn_with_state(state, require_auth))
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

/// Resolve `[session:id]` and `[report:folder/file]` references in text.
pub(super) async fn resolve_references(text: &str, state: &Arc<AppState>) -> String {
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
