//! REST surface for peckboard-managed background tasks
//! ([`crate::background`]):
//!
//! - `GET  /api/sessions/{id}/background` — the session's tasks.
//! - `GET  /api/background/{task_id}/log?lines=N` — task + last N output lines.
//! - `POST /api/background/{task_id}/stop` — stop a running task.
//!
//! Access follows the session's: the per-session route sits behind
//! `require_session_access`; the per-task routes resolve the task's session
//! and apply the same `may_access_session` rule, answering 404 (never 403)
//! so a task id can't be used as an existence oracle.

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    routing::{get, post},
};
use serde::Deserialize;
use std::sync::Arc;

use crate::auth::middleware::{AuthUser, require_auth, require_session_access};
use crate::background::TaskInfo;
use crate::state::AppState;

type ApiError = (StatusCode, Json<serde_json::Value>);

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let session_scoped = Router::new()
        .route("/api/sessions/{id}/background", get(list_session_tasks))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_session_access,
        ));
    Router::new()
        .route("/api/background/{task_id}/log", get(task_log))
        .route("/api/background/{task_id}/stop", post(stop_task))
        .merge(session_scoped)
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

#[derive(Deserialize)]
struct LogQuery {
    lines: Option<usize>,
}

fn not_found() -> ApiError {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "background task not found" })),
    )
}

/// The task, if it exists and the caller may access its session.
async fn authorized_task(
    state: &AppState,
    user: &AuthUser,
    task_id: &str,
) -> Result<TaskInfo, ApiError> {
    let info = state.background.get(task_id).ok_or_else(not_found)?;
    let session = state
        .db
        .get_session(&info.session_id)
        .await
        .ok()
        .flatten()
        .ok_or_else(not_found)?;
    if !crate::auth::access::may_access_session(
        user.is_admin(),
        &user.user_id,
        session.user_id.as_deref(),
        session.project_id.as_deref(),
    ) {
        return Err(not_found());
    }
    Ok(info)
}

/// GET /api/sessions/{id}/background → `{"tasks": [TaskInfo..]}`, oldest first.
async fn list_session_tasks(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "tasks": state.background.list_for_session(&id) }))
}

/// GET /api/background/{task_id}/log?lines=N → `{"task": TaskInfo, "lines": [..]}`.
/// `lines` defaults to 200 (the in-memory ring), max 2000.
async fn task_log(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(task_id): Path<String>,
    Query(q): Query<LogQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let info = authorized_task(&state, &user, &task_id).await?;
    let lines = state
        .background
        .tail(&task_id, q.lines.unwrap_or(crate::background::TAIL_LINES))
        .unwrap_or_default();
    Ok(Json(serde_json::json!({ "task": info, "lines": lines })))
}

/// POST /api/background/{task_id}/stop → `{"task": TaskInfo}` (with
/// `stopping: true`); 409 when the task already finished.
async fn stop_task(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(task_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    authorized_task(&state, &user, &task_id).await?;
    let info = state.background.stop(&task_id).map_err(|e| {
        (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": e })),
        )
    })?;
    Ok(Json(serde_json::json!({ "task": info })))
}
