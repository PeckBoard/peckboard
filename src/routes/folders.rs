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
}

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/folders", post(create_folder).get(list_folders))
        .route("/api/folders/{id}", delete(delete_folder))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// POST /api/folders
async fn create_folder(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateFolderRequest>,
) -> impl IntoResponse {
    // Validate that the path exists on disk
    let path = std::path::Path::new(&body.path);
    if !path.exists() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("path does not exist: {}", body.path) })),
        ));
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
    // Check if sessions exist for this folder
    let sessions = state.db.list_sessions_by_folder(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    if !sessions.is_empty() {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "folder has active sessions",
                "session_count": sessions.len()
            })),
        ));
    }

    let deleted = state.db.delete_folder(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    if !deleted {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "folder not found" })),
        ));
    }

    Ok(StatusCode::NO_CONTENT)
}
