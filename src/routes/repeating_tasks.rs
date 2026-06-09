//! HTTP routes for repeating tasks.
//!
//! Every dispatch path goes through `RepeatingTaskManager::try_run_now`
//! which acquires the per-task lock before deciding to spawn a session.
//! Routes never call `db.create_session()` directly for a task — that
//! invariant lives in `crate::repeating::RepeatingTaskManager`.

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
use crate::db::models::{NewRepeatingTask, UpdateRepeatingTask};
use crate::repeating::{RunContext, Schedule, StartOutcome, initial_next_run_at};
use crate::state::AppState;

#[derive(Deserialize)]
pub struct CreateRepeatingTaskRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub folder_id: String,
    pub prompt: String,
    pub schedule_kind: String,
    pub schedule_value: serde_json::Value,
    pub model: Option<String>,
    pub effort: Option<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

#[derive(Deserialize)]
pub struct UpdateRepeatingTaskRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub folder_id: Option<String>,
    pub prompt: Option<String>,
    pub schedule_kind: Option<String>,
    pub schedule_value: Option<serde_json::Value>,
    pub model: Option<Option<String>>,
    pub effort: Option<Option<String>>,
    pub enabled: Option<bool>,
}

#[derive(Deserialize)]
pub struct ListQuery {
    pub folder_id: Option<String>,
}

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/repeating-tasks",
            post(create_repeating_task).get(list_repeating_tasks),
        )
        .route(
            "/api/repeating-tasks/{id}",
            get(get_repeating_task)
                .patch(update_repeating_task)
                .delete(delete_repeating_task),
        )
        .route("/api/repeating-tasks/{id}/run", post(run_repeating_task))
        .route(
            "/api/repeating-tasks/{id}/sessions",
            get(list_repeating_task_sessions),
        )
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

fn bad_request(msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": msg.into() })),
    )
}

fn internal_error(err: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": err.to_string() })),
    )
}

fn not_found() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "repeating task not found" })),
    )
}

fn validate_name(name: &str) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(bad_request("name is required"));
    }
    if trimmed.len() > 200 {
        return Err(bad_request("name is too long (max 200)"));
    }
    Ok(trimmed.to_string())
}

fn validate_prompt(prompt: &str) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    if prompt.trim().is_empty() {
        return Err(bad_request("prompt is required"));
    }
    // Cap the prompt to a generous-but-finite size. Without a bound an
    // attacker (or accidental paste) can bloat the DB and slow every
    // scheduler tick that loads the row.
    if prompt.len() > 200_000 {
        return Err(bad_request("prompt is too long (max 200000 bytes)"));
    }
    Ok(prompt.to_string())
}

fn validate_schedule(
    kind: &str,
    value: &serde_json::Value,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    let value_str = serde_json::to_string(value).map_err(internal_error)?;
    Schedule::parse(kind, &value_str).map_err(|e| bad_request(e.to_string()))?;
    Ok(value_str)
}

async fn create_repeating_task(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateRepeatingTaskRequest>,
) -> impl IntoResponse {
    let name = validate_name(&body.name)?;
    let prompt = validate_prompt(&body.prompt)?;
    let schedule_value = validate_schedule(&body.schedule_kind, &body.schedule_value)?;

    // Verify folder exists; without this the task row would reference a
    // non-existent folder and every dispatch would error out at run time.
    if state
        .db
        .get_folder(&body.folder_id)
        .await
        .map_err(internal_error)?
        .is_none()
    {
        return Err(bad_request("folder not found"));
    }

    let now = chrono::Utc::now().to_rfc3339();
    let id = uuid::Uuid::new_v4().to_string();

    // Construct a draft to compute next_run_at — we don't have a stored
    // row yet but next_run_at_after only cares about kind+value.
    let draft = crate::db::models::RepeatingTask {
        id: id.clone(),
        name: name.clone(),
        description: body.description.clone(),
        folder_id: body.folder_id.clone(),
        prompt: prompt.clone(),
        schedule_kind: body.schedule_kind.clone(),
        schedule_value: schedule_value.clone(),
        model: body.model.clone(),
        effort: body.effort.clone(),
        enabled: body.enabled,
        next_run_at: None,
        last_run_at: None,
        created_at: now.clone(),
        updated_at: now.clone(),
    };
    // Disabled tasks must not surface in the due-task scan, so leave
    // next_run_at empty until the task is enabled.
    let next_run_at = if body.enabled {
        initial_next_run_at(&draft)
    } else {
        None
    };

    let task = state
        .db
        .create_repeating_task(NewRepeatingTask {
            id,
            name,
            description: body.description,
            folder_id: body.folder_id,
            prompt,
            schedule_kind: body.schedule_kind,
            schedule_value,
            model: body.model,
            effort: body.effort,
            enabled: body.enabled,
            next_run_at,
            last_run_at: None,
            created_at: now.clone(),
            updated_at: now,
        })
        .await
        .map_err(internal_error)?;

    state
        .broadcaster
        .broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "repeating-task-changed".into(),
            session_id: task.id.clone(),
            data: serde_json::json!({ "action": "created", "task": &task }),
        });

    Ok::<_, (StatusCode, Json<serde_json::Value>)>((
        StatusCode::CREATED,
        Json(serde_json::json!(task)),
    ))
}

async fn list_repeating_tasks(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ListQuery>,
) -> impl IntoResponse {
    let tasks = match q.folder_id {
        Some(fid) => state.db.list_repeating_tasks_by_folder(&fid).await,
        None => state.db.list_repeating_tasks().await,
    }
    .map_err(internal_error)?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!(tasks)))
}

async fn get_repeating_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state
        .db
        .get_repeating_task(&id)
        .await
        .map_err(internal_error)?
    {
        Some(t) => Ok(Json(serde_json::json!(t))),
        None => Err(not_found()),
    }
}

async fn update_repeating_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<UpdateRepeatingTaskRequest>,
) -> impl IntoResponse {
    let existing = state
        .db
        .get_repeating_task(&id)
        .await
        .map_err(internal_error)?
        .ok_or_else(not_found)?;

    let name = match body.name.as_deref() {
        Some(n) => Some(validate_name(n)?),
        None => None,
    };
    let prompt = match body.prompt.as_deref() {
        Some(p) => Some(validate_prompt(p)?),
        None => None,
    };

    // Schedule kind/value must be edited together so we can re-validate.
    // If only one is supplied, fall back to the existing other field.
    let (schedule_kind, schedule_value) = match (&body.schedule_kind, &body.schedule_value) {
        (None, None) => (None, None),
        (Some(k), Some(v)) => (Some(k.clone()), Some(validate_schedule(k, v)?)),
        (Some(k), None) => {
            let existing_val = &existing.schedule_value;
            let parsed: serde_json::Value =
                serde_json::from_str(existing_val).map_err(internal_error)?;
            (Some(k.clone()), Some(validate_schedule(k, &parsed)?))
        }
        (None, Some(v)) => (
            Some(existing.schedule_kind.clone()),
            Some(validate_schedule(&existing.schedule_kind, v)?),
        ),
    };

    if let Some(ref fid) = body.folder_id {
        if state
            .db
            .get_folder(fid)
            .await
            .map_err(internal_error)?
            .is_none()
        {
            return Err(bad_request("folder not found"));
        }
    }

    let now = chrono::Utc::now().to_rfc3339();

    // Recompute next_run_at when schedule or enabled flag changes. We
    // don't disturb it on a pure name/prompt edit so a long interval
    // task doesn't get reset by an unrelated description tweak.
    let recompute_next =
        schedule_kind.is_some() || schedule_value.is_some() || body.enabled.is_some();
    let mut next_run_at_update: Option<Option<String>> = None;
    if recompute_next {
        let draft = crate::db::models::RepeatingTask {
            id: existing.id.clone(),
            name: name.clone().unwrap_or(existing.name.clone()),
            description: body
                .description
                .clone()
                .unwrap_or(existing.description.clone()),
            folder_id: body.folder_id.clone().unwrap_or(existing.folder_id.clone()),
            prompt: prompt.clone().unwrap_or(existing.prompt.clone()),
            schedule_kind: schedule_kind
                .clone()
                .unwrap_or(existing.schedule_kind.clone()),
            schedule_value: schedule_value
                .clone()
                .unwrap_or(existing.schedule_value.clone()),
            model: existing.model.clone(),
            effort: existing.effort.clone(),
            enabled: body.enabled.unwrap_or(existing.enabled),
            next_run_at: None,
            last_run_at: existing.last_run_at.clone(),
            created_at: existing.created_at.clone(),
            updated_at: now.clone(),
        };
        if draft.enabled {
            next_run_at_update = Some(initial_next_run_at(&draft));
        } else {
            // Disabled tasks shouldn't surface in the due query.
            next_run_at_update = Some(None);
        }
    }

    let update = UpdateRepeatingTask {
        name,
        description: body.description,
        folder_id: body.folder_id,
        prompt,
        schedule_kind,
        schedule_value,
        model: body.model,
        effort: body.effort,
        enabled: body.enabled,
        next_run_at: next_run_at_update,
        last_run_at: None,
        updated_at: Some(now),
    };

    let updated = state
        .db
        .update_repeating_task(&id, update)
        .await
        .map_err(internal_error)?
        .ok_or_else(not_found)?;

    state
        .broadcaster
        .broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "repeating-task-changed".into(),
            session_id: updated.id.clone(),
            data: serde_json::json!({ "action": "updated", "task": &updated }),
        });

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!(updated)))
}

async fn delete_repeating_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let deleted = state
        .db
        .delete_repeating_task(&id)
        .await
        .map_err(internal_error)?;
    if !deleted {
        return Err(not_found());
    }
    state
        .broadcaster
        .broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "repeating-task-changed".into(),
            session_id: id.clone(),
            data: serde_json::json!({ "action": "deleted", "id": id }),
        });
    Ok::<_, (StatusCode, Json<serde_json::Value>)>(StatusCode::NO_CONTENT)
}

/// POST /api/repeating-tasks/:id/run -- force run. The lock is acquired
/// by `try_run_now`, so concurrent force-run + scheduler-tick can never
/// double-spawn.
async fn run_repeating_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let outcome = state
        .repeating_task_manager
        .try_run_now(
            &id,
            RunContext {
                db: &state.db,
                broadcaster: &state.broadcaster,
                session_manager: &state.session_manager,
                mcp_tokens: &state.mcp_tokens,
                data_dir: &state.config.data_dir,
                http_port: state.config.port,
            },
            false, // force-run ignores the `enabled` flag
        )
        .await
        .map_err(internal_error)?;

    let status = match outcome {
        StartOutcome::Spawned => "spawned",
        StartOutcome::AlreadyRunning => "already_running",
        StartOutcome::Disabled => "disabled",
    };
    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({ "status": status })))
}

async fn list_repeating_task_sessions(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if state
        .db
        .get_repeating_task(&id)
        .await
        .map_err(internal_error)?
        .is_none()
    {
        return Err(not_found());
    }
    let sessions = state
        .db
        .list_sessions_by_repeating_task(&id)
        .await
        .map_err(internal_error)?;
    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!(sessions)))
}
