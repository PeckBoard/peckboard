use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{delete, post},
};
use serde::Deserialize;
use std::sync::Arc;

use crate::auth::middleware::require_auth;
use crate::db::models::NewFolder;
use crate::state::AppState;

#[derive(Deserialize)]
struct CreateFolderRequest {
    name: String,
    path: String,
    /// If true, create the directory on disk if it doesn't exist.
    create: Option<bool>,
}

#[derive(Deserialize)]
struct MoveSessionsRequest {
    target_folder_id: String,
}

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/folders", post(create_folder).get(list_folders))
        .route("/api/folders/{id}", delete(delete_folder))
        .route(
            "/api/folders/{id}/delete-sessions",
            post(delete_with_sessions),
        )
        .route(
            "/api/folders/{id}/move-sessions",
            post(move_sessions_then_delete),
        )
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// POST /api/folders
async fn create_folder(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateFolderRequest>,
) -> impl IntoResponse {
    tracing::info!(name = %body.name, path = %body.path, create = body.create.unwrap_or(false), "Creating folder");
    let path = std::path::Path::new(&body.path);
    if !path.exists() {
        if body.create.unwrap_or(false) {
            // Create the directory
            if let Err(e) = std::fs::create_dir_all(path) {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(
                        serde_json::json!({ "error": format!("failed to create directory: {}", e) }),
                    ),
                ));
            }
            tracing::info!(path = %body.path, "Created directory on disk");
        } else {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(
                    serde_json::json!({ "error": format!("path does not exist: {}. Set create: true to create it.", body.path) }),
                ),
            ));
        }
    }

    let now = chrono::Utc::now().to_rfc3339();
    let id = uuid::Uuid::new_v4().to_string();

    let folder = state
        .db
        .create_folder(NewFolder {
            id,
            name: body.name,
            path: body.path,
            created_at: now,
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    Ok((StatusCode::CREATED, Json(serde_json::json!(folder))))
}

/// GET /api/folders
async fn list_folders(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    tracing::info!("Listing folders");
    let folders = state.db.list_folders().await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!(folders)))
}

/// DELETE /api/folders/:id
async fn delete_folder(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(folder_id = %id, "Deleting folder");

    // Atomic check-and-delete: prevents a concurrent session creation
    // from slipping in between an empty-check and the delete.
    let outcome = state.db.delete_folder_if_empty(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    match outcome {
        crate::db::crud::FolderEmptyDelete::Deleted => Ok(StatusCode::NO_CONTENT),
        crate::db::crud::FolderEmptyDelete::HasSessions(n) => Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "folder has active sessions",
                "session_count": n,
            })),
        )),
        crate::db::crud::FolderEmptyDelete::NotFound => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "folder not found" })),
        )),
    }
}

/// POST /api/folders/:id/delete-sessions — delete all sessions in folder, then delete folder
async fn delete_with_sessions(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(folder_id = %id, "Cascade-deleting folder and sessions");

    // Atomic cascade: sessions, their events, queued messages, and the
    // folder all drop in a single transactional closure. Replaces the
    // older "list → loop with let _ = …" pattern that silently dropped
    // failures and could leave orphaned rows behind.
    let report = state.db.delete_folder_cascade(&id).await.map_err(|e| {
        let msg = e.to_string();
        let status = if msg.contains("not found") {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        (status, Json(serde_json::json!({ "error": msg })))
    })?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({
        "deleted_sessions": report.sessions_deleted,
        "deleted_events": report.events_deleted,
    })))
}

/// POST /api/folders/:id/move-sessions — move sessions to target folder, then delete folder
async fn move_sessions_then_delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<MoveSessionsRequest>,
) -> impl IntoResponse {
    // Verify target folder exists
    let target = state
        .db
        .get_folder(&body.target_folder_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    if target.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "target folder not found" })),
        ));
    }

    // Move all sessions to target folder
    let moved = state
        .db
        .move_sessions_to_folder(&id, &body.target_folder_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    // Delete the now-empty folder
    state.db.delete_folder(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({
        "moved_sessions": moved
    })))
}
