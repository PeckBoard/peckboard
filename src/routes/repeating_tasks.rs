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
use crate::repeating::{
    RunContext, Schedule, StartOutcome, TARGET_ASSISTANT, initial_next_run_at, server_timezone,
};
use crate::routes::misc::explicit_null;
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
    /// IANA zone name (e.g. "America/New_York"). `None` means UTC —
    /// also the behaviour for every task created before this field
    /// existed.
    #[serde(default)]
    pub timezone: Option<String>,
    /// `"session"` (default) or `"assistant"`: where each run happens.
    #[serde(default)]
    pub target: Option<String>,
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
    /// Explicit `null` clears the effort override (see `explicit_null`);
    /// absent leaves it alone. Without this the edit form's "Default"
    /// choice was silently dropped.
    #[serde(default, deserialize_with = "explicit_null")]
    pub effort: Option<Option<String>>,
    pub enabled: Option<bool>,
    pub timezone: Option<Option<String>>,
    /// `"session"` (or `null`) = a fresh session per run; `"assistant"` =
    /// the Assistant performs it. Absent leaves it alone.
    #[serde(default, deserialize_with = "explicit_null")]
    pub target: Option<Option<String>>,
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
        .route(
            "/api/repeating-tasks/{id}/runs",
            get(list_repeating_task_runs),
        )
        .route(
            "/api/repeating-tasks/presets/morning-briefing",
            post(create_morning_briefing),
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

/// `None` means UTC; anything else must be a valid IANA zone name so a
/// typo doesn't silently fall back to UTC for the schedule's lifetime.
fn validate_timezone(
    tz: Option<&str>,
) -> Result<Option<String>, (StatusCode, Json<serde_json::Value>)> {
    match tz {
        None => Ok(None),
        Some(z) => z
            .parse::<chrono_tz::Tz>()
            .map(|_| Some(z.to_string()))
            .map_err(|_| bad_request(format!("unknown timezone: {z}"))),
    }
}

/// `None`/`"session"` = a fresh session per run (stored as NULL);
/// `"assistant"` = the Assistant performs it.
fn validate_target(
    target: Option<&str>,
) -> Result<Option<String>, (StatusCode, Json<serde_json::Value>)> {
    match target {
        None | Some("session") => Ok(None),
        Some(TARGET_ASSISTANT) => Ok(Some(TARGET_ASSISTANT.to_string())),
        Some(other) => Err(bad_request(format!(
            "unknown target: {other} (use session or assistant)"
        ))),
    }
}

async fn create_repeating_task(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateRepeatingTaskRequest>,
) -> impl IntoResponse {
    let name = validate_name(&body.name)?;
    let prompt = validate_prompt(&body.prompt)?;
    let schedule_value = validate_schedule(&body.schedule_kind, &body.schedule_value)?;
    let timezone = validate_timezone(body.timezone.as_deref())?;
    let target = validate_target(body.target.as_deref())?;
    crate::routes::settings::check_model_or_400(body.model.as_deref())?;

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
    // row yet but next_run_at_after only cares about kind+value+timezone.
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
        timezone: timezone.clone(),
        target: target.clone(),
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
            timezone,
            target,
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
    let timezone = match &body.timezone {
        Some(tz) => Some(validate_timezone(tz.as_deref())?),
        None => None,
    };
    let target = match &body.target {
        Some(t) => Some(validate_target(t.as_deref())?),
        None => None,
    };
    crate::routes::settings::check_model_or_400(body.model.as_ref().and_then(|m| m.as_deref()))?;

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

    // Recompute next_run_at when schedule, timezone, or enabled flag
    // changes. We don't disturb it on a pure name/prompt edit so a long
    // interval task doesn't get reset by an unrelated description tweak.
    let recompute_next = schedule_kind.is_some()
        || schedule_value.is_some()
        || timezone.is_some()
        || body.enabled.is_some();
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
            timezone: timezone.clone().unwrap_or(existing.timezone.clone()),
            target: existing.target.clone(),
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
        timezone,
        target,
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
                auditor: &state.run_auditor,
            },
            false, // force-run ignores the `enabled` flag
        )
        .await
        .map_err(internal_error)?;

    let (status, reason): (&str, Option<String>) = match outcome {
        StartOutcome::Spawned => ("spawned", None),
        StartOutcome::AlreadyRunning => ("already_running", None),
        StartOutcome::Disabled => ("disabled", None),
        // A spent `once` schedule is a refusal, not an outcome to report
        // as a 200: the task fired already and must not fire again. 409
        // so the store's `!res.ok` path surfaces the message as run
        // status text in the UI.
        StartOutcome::ConsumedOnce => {
            return Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "This one-off task has already run. Edit its scheduled time to run it again.",
                })),
            ));
        }
        // The force-run route never throttles (Manual trigger always
        // passes the policy check), so this branch is unreachable in
        // practice. We still spell it out so any future code that
        // wires force-run to Scheduler can't silently drop the reason.
        StartOutcome::Throttled(r) => ("throttled", Some(r)),
        StartOutcome::QueuedForAssistant => ("queued", None),
        StartOutcome::NoAssistant => {
            return Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "There is no Assistant yet. Open the Assistant page once, then run this task again.",
                })),
            ));
        }
    };
    let body = match reason {
        Some(r) => serde_json::json!({ "status": status, "reason": r }),
        None => serde_json::json!({ "status": status }),
    };
    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(body))
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

/// GET /api/repeating-tasks/:id/runs -- newest-first dispatch history.
async fn list_repeating_task_runs(
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
    let runs = state
        .db
        .list_repeating_task_runs(&id, 50)
        .await
        .map_err(internal_error)?;
    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!(runs)))
}

/// Name of the morning-briefing preset (also how an existing one is found).
pub const MORNING_BRIEFING_NAME: &str = "Morning briefing";

/// What the Assistant is asked to do each morning.
pub const MORNING_BRIEFING_PROMPT: &str = "Give the user their morning briefing. \
Cover, briefly and by name, only what has news since yesterday evening: sessions that \
finished, failed, or are waiting on the user overnight (list_sessions, then read the ones \
that changed); cards that moved on the project boards (list_cards); new reports \
(list_project_reports); printer status and any print that finished or failed; and the \
home status (lights or devices left on, anything unusual), where those tools are \
available. Skip anything with no news, then ask whether they want to dig into any of it.";

#[derive(Deserialize, Default)]
pub struct MorningBriefingRequest {
    /// Local wall-clock time; defaults to 07:30.
    pub hour: Option<u32>,
    pub minute: Option<u32>,
    /// The browser's IANA zone; defaults to the server's own zone.
    pub timezone: Option<String>,
}

/// POST /api/repeating-tasks/presets/morning-briefing -- create the daily
/// Assistant-target "Morning briefing" task in the Assistant's folder.
/// Idempotent: an existing Assistant-target task of that name is returned
/// as is (200) instead of being duplicated (201 when created).
async fn create_morning_briefing(
    State(state): State<Arc<AppState>>,
    Json(body): Json<MorningBriefingRequest>,
) -> impl IntoResponse {
    let hour = body.hour.unwrap_or(7);
    let minute = body.minute.unwrap_or(30);
    let schedule_value = validate_schedule(
        "daily",
        &serde_json::json!({ "hour": hour, "minute": minute }),
    )?;
    let timezone = match validate_timezone(body.timezone.as_deref())? {
        Some(z) => Some(z),
        None => server_timezone(),
    };
    let voice = state
        .db
        .find_expert_session(crate::service::voice_relay::VOICE_EXPERT_KIND)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| {
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "There is no Assistant yet. Open the Assistant page once, then add the briefing.",
                })),
            )
        })?;
    let existing = state
        .db
        .list_repeating_tasks()
        .await
        .map_err(internal_error)?
        .into_iter()
        .find(|t| t.name == MORNING_BRIEFING_NAME && t.target.as_deref() == Some(TARGET_ASSISTANT));
    if let Some(task) = existing {
        return Ok((StatusCode::OK, Json(serde_json::json!(task))));
    }

    let now = chrono::Utc::now().to_rfc3339();
    let id = uuid::Uuid::new_v4().to_string();
    let draft = crate::db::models::RepeatingTask {
        id: id.clone(),
        name: MORNING_BRIEFING_NAME.to_string(),
        description: String::new(),
        folder_id: voice.folder_id.clone(),
        prompt: MORNING_BRIEFING_PROMPT.to_string(),
        schedule_kind: "daily".to_string(),
        schedule_value: schedule_value.clone(),
        model: None,
        effort: None,
        enabled: true,
        next_run_at: None,
        last_run_at: None,
        created_at: now.clone(),
        updated_at: now.clone(),
        timezone: timezone.clone(),
        target: Some(TARGET_ASSISTANT.to_string()),
    };
    let task = state
        .db
        .create_repeating_task(NewRepeatingTask {
            id,
            name: draft.name.clone(),
            description: String::new(),
            folder_id: voice.folder_id,
            prompt: draft.prompt.clone(),
            schedule_kind: draft.schedule_kind.clone(),
            schedule_value,
            model: None,
            effort: None,
            enabled: true,
            next_run_at: initial_next_run_at(&draft),
            last_run_at: None,
            created_at: now.clone(),
            updated_at: now,
            timezone,
            target: draft.target.clone(),
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
