//! Project and card HTTP routes. Project handlers live here; card
//! handlers (create/list/update/delete/stop/restart/cancel/reports)
//! live in [`cards`]. Shared dependency-graph helpers stay in this
//! module so both layers can call them without re-exporting.

mod cards;
mod pm;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{get, post, put},
};
use serde::Deserialize;
use std::sync::Arc;

use crate::auth::middleware::require_auth;
use crate::db::models::{Card, NewProject, UpdateProject};
use crate::state::AppState;
use crate::workflow;

// ── Request / query types ───────────────────────────────────────────

#[derive(Deserialize)]
struct CreateProjectRequest {
    name: String,
    folder_id: String,
    #[serde(default)]
    context: String,
    #[serde(default = "default_worker_count")]
    worker_count: i32,
    model: Option<String>,
    effort: Option<String>,
    workflow: Option<String>,
    #[serde(default)]
    parallel_instructions: bool,
    #[serde(default = "default_true")]
    auto_notify_changes: bool,
    #[serde(default = "default_true")]
    worker_communication: bool,
}

fn default_worker_count() -> i32 {
    1
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
struct ListProjectsQuery {
    folder_id: Option<String>,
}

#[derive(Deserialize)]
struct UpdateProjectRequest {
    name: Option<String>,
    context: Option<String>,
    worker_count: Option<i32>,
    status: Option<String>,
    workflow: Option<String>,
    model: Option<Option<String>>,
    effort: Option<Option<String>>,
    parallel_instructions: Option<bool>,
    auto_notify_changes: Option<bool>,
    worker_communication: Option<bool>,
}

// ── Shared error type + helpers ─────────────────────────────────────

pub(super) type RouteError = (StatusCode, Json<serde_json::Value>);

pub(super) fn bad_request(msg: impl Into<String>) -> RouteError {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": msg.into() })),
    )
}

pub(super) fn internal_error(e: impl std::fmt::Display) -> RouteError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": e.to_string() })),
    )
}

// ── Card dependency helpers ─────────────────────────────────────────

/// Would setting `card_id`'s dependencies to `new_deps` introduce a
/// cycle, given the project's existing edges (each card mapped to the
/// cards it depends on)? Walk outward from `new_deps`; if we can reach
/// `card_id` again, the new edges would close a loop.
fn would_create_cycle(
    edges: &std::collections::HashMap<String, Vec<String>>,
    card_id: &str,
    new_deps: &[String],
) -> bool {
    let mut stack: Vec<&str> = new_deps.iter().map(|s| s.as_str()).collect();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    while let Some(node) = stack.pop() {
        if node == card_id {
            return true;
        }
        if !seen.insert(node) {
            continue;
        }
        if let Some(next) = edges.get(node) {
            stack.extend(next.iter().map(|s| s.as_str()));
        }
    }
    false
}

/// Validate a proposed dependency set for `card_id` and persist it.
/// Rejects dependencies that aren't cards in the same project and any set
/// that would form a cycle. An empty set clears the card's dependencies.
pub(super) async fn apply_dependencies(
    state: &AppState,
    project_id: &str,
    card_id: &str,
    depends_on: Vec<String>,
) -> Result<(), RouteError> {
    // Drop self-references and duplicates.
    let mut deps: Vec<String> = Vec::new();
    for d in depends_on {
        if d != card_id && !deps.contains(&d) {
            deps.push(d);
        }
    }

    if !deps.is_empty() {
        let project_cards = state
            .db
            .list_cards_by_project(project_id)
            .await
            .map_err(internal_error)?;
        let valid_ids: std::collections::HashSet<&str> =
            project_cards.iter().map(|c| c.id.as_str()).collect();
        for d in &deps {
            if !valid_ids.contains(d.as_str()) {
                return Err(bad_request(format!(
                    "dependency {d} is not a card in this project"
                )));
            }
        }

        let existing = state
            .db
            .list_dependencies_by_project(project_id)
            .await
            .map_err(internal_error)?;
        let mut edges: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for (c, dep) in existing {
            edges.entry(c).or_default().push(dep);
        }
        if would_create_cycle(&edges, card_id, &deps) {
            return Err(bad_request(
                "dependency cycle detected — a card cannot transitively depend on itself",
            ));
        }
    }

    state
        .db
        .set_card_dependencies(card_id, deps)
        .await
        .map_err(internal_error)?;
    Ok(())
}

/// Serialize a card with its `depends_on` ids attached, so the frontend
/// can render dependency state without a second round-trip.
pub(super) async fn card_json_with_deps(state: &AppState, card: &Card) -> serde_json::Value {
    let deps = state
        .db
        .list_card_dependencies(&card.id)
        .await
        .unwrap_or_default();
    let mut value = serde_json::to_value(card).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(obj) = value.as_object_mut() {
        obj.insert("depends_on".into(), serde_json::json!(deps));
    }
    value
}

// ── Router ──────────────────────────────────────────────────────────

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/projects", post(create_project).get(list_projects))
        .route(
            "/api/projects/{id}",
            get(get_project).put(update_project).delete(delete_project),
        )
        .route("/api/projects/{id}/pause", post(pause_project))
        .route("/api/projects/{id}/resume", post(resume_project))
        .route(
            "/api/projects/{id}/cards",
            post(cards::create_card).get(cards::list_cards),
        )
        .route(
            "/api/projects/{id}/cards/{card_id}",
            put(cards::update_card).delete(cards::delete_card),
        )
        .route(
            "/api/projects/{id}/cards/{card_id}/stop",
            post(cards::stop_card_worker),
        )
        .route(
            "/api/projects/{id}/cards/{card_id}/restart",
            post(cards::restart_card_worker),
        )
        .route(
            "/api/projects/{id}/cards/{card_id}/cancel-wont-do",
            post(cards::cancel_card_wont_do),
        )
        .route(
            "/api/projects/{id}/cards/{card_id}/reports",
            get(cards::list_card_reports),
        )
        .route(
            "/api/projects/{id}/pending-questions",
            get(list_pending_questions),
        )
        .route(
            "/api/projects/{id}/pm/decisions",
            get(pm::list_pm_decisions),
        )
        .route(
            "/api/projects/{id}/pm/decisions/{did}",
            put(pm::update_pm_decision),
        )
        .route(
            "/api/projects/{id}/pm/questions",
            get(pm::list_pm_questions),
        )
        .route(
            "/api/projects/{id}/pm/questions/{qid}/answer",
            post(pm::answer_pm_question),
        )
        .route("/api/projects/{id}/todos", get(list_project_todos))
        .route(
            "/api/projects/{id}/workflow-instructions",
            get(list_workflow_instructions).put(upsert_workflow_instruction),
        )
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

// ── Project handlers ────────────────────────────────────────────────

/// POST /api/projects
async fn create_project(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateProjectRequest>,
) -> impl IntoResponse {
    tracing::info!(name = %body.name, folder_id = %body.folder_id, "Creating project");

    // A project's workflow is a required (NOT NULL) column. Reject
    // create requests that omit it or name an unknown workflow.
    let workflow_id = match body.workflow.as_deref().map(str::trim) {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => return Err(bad_request("workflow is required")),
    };
    if crate::workflow::workflow_by_id(&workflow_id).is_none() {
        return Err(bad_request(format!("unknown workflow id '{workflow_id}'")));
    }

    let now = chrono::Utc::now().to_rfc3339();
    let id = uuid::Uuid::new_v4().to_string();

    let project = state
        .db
        .create_project(NewProject {
            id,
            name: body.name,
            context: body.context,
            folder_id: body.folder_id,
            worker_count: body.worker_count,
            status: "active".to_string(),
            workflow: workflow_id,
            model: body.model,
            effort: body.effort,
            parallel_instructions: body.parallel_instructions,
            auto_notify_changes: body.auto_notify_changes,
            worker_communication: body.worker_communication,
            created_at: now.clone(),
            last_accessed_at: now,
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    // Idempotently give the new project its own question-expert. Non-fatal:
    // a failure here must not block project creation, and unscoped callers
    // still fall back to the global question-expert.
    if let Err(e) =
        crate::service::question_expert::ensure_project_question_expert(&state.db, &project).await
    {
        tracing::warn!(project_id = %project.id, "Failed to ensure project question-expert: {e}");
    }

    // Likewise its PM expert (durable store of project-direction decisions).
    if let Err(e) = crate::service::pm_expert::ensure_project_pm_expert(&state.db, &project).await {
        tracing::warn!(project_id = %project.id, "Failed to ensure project PM expert: {e}");
    }

    Ok::<_, (StatusCode, Json<serde_json::Value>)>((
        StatusCode::CREATED,
        Json(serde_json::json!(project)),
    ))
}

/// GET /api/projects
async fn list_projects(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListProjectsQuery>,
) -> impl IntoResponse {
    tracing::info!(folder_id = ?params.folder_id, "Listing projects");
    let projects = if let Some(folder_id) = params.folder_id {
        state.db.list_projects_by_folder(&folder_id).await
    } else {
        state.db.list_projects().await
    };

    let projects = projects.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!(projects)))
}

/// GET /api/projects/:id
async fn get_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(project_id = %id, "Getting project");
    let project = state.db.get_project(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    let project = match project {
        Some(p) => p,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "project not found" })),
            ));
        }
    };

    let cards = state.db.list_cards_by_project(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    Ok(Json(serde_json::json!({
        "project": project,
        "cards": cards,
    })))
}

/// PUT /api/projects/:id
async fn update_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<UpdateProjectRequest>,
) -> impl IntoResponse {
    tracing::info!(project_id = %id, "Updating project");

    // A project's workflow is required (NOT NULL). Omitting the field
    // is fine — other updates can land without touching the workflow.
    // An explicit empty string or unknown id is rejected.
    let workflow = match body.workflow {
        Some(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Err(bad_request("workflow is required"));
            }
            if crate::workflow::workflow_by_id(trimmed).is_none() {
                return Err(bad_request(format!("unknown workflow id '{trimmed}'")));
            }
            Some(trimmed.to_string())
        }
        None => None,
    };

    // Generic PUT is also the path the KanbanBoard's "Resume" menu hits
    // (it sends `{status: "active"}`). Treat any status flip to active as
    // an implicit "I've acknowledged the auto-pause reason" and clear the
    // banner so a stale message doesn't linger forever.
    let clear_pause_reason = body.status.as_deref() == Some("active");

    let update = UpdateProject {
        name: body.name,
        context: body.context,
        worker_count: body.worker_count,
        status: body.status,
        workflow,
        model: body.model,
        effort: body.effort,
        parallel_instructions: body.parallel_instructions,
        auto_notify_changes: body.auto_notify_changes,
        worker_communication: body.worker_communication,
        last_accessed_at: Some(chrono::Utc::now().to_rfc3339()),
        pause_reason: if clear_pause_reason { Some(None) } else { None },
    };

    let project = state.db.update_project(&id, update).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    match project {
        Some(p) => Ok(Json(serde_json::json!(p))),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "project not found" })),
        )),
    }
}

/// DELETE /api/projects/:id
async fn delete_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(project_id = %id, "Deleting project");

    // Atomic cascade: cards, their worker sessions, those sessions'
    // events and queued messages, and the project itself all delete in
    // one closure under the DB connection mutex. Replaces the old
    // pattern that did each step in a separate await with `let _ = …`
    // swallowing errors, which could leave orphans and silently report
    // success when the cascade was only half-applied.
    let report = state.db.delete_project_cascade(&id).await.map_err(|e| {
        let msg = e.to_string();
        let status = if msg.contains("not found") {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        (status, Json(serde_json::json!({ "error": msg })))
    })?;
    tracing::info!(
        project_id = %id,
        cards = report.cards_deleted,
        sessions = report.sessions_deleted,
        events = report.events_deleted,
        "Project cascade-deleted"
    );

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(StatusCode::NO_CONTENT)
}

/// POST /api/projects/:id/pause
async fn pause_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(project_id = %id, "Pausing project");
    // Flip status first so the orchestrator's 5s tick won't immediately
    // re-spawn workers in the window between cancel and the user
    // noticing.
    let update = UpdateProject {
        status: Some("paused".to_string()),
        last_accessed_at: Some(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };

    let project = state.db.update_project(&id, update).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    let project = match project {
        Some(p) => p,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "project not found" })),
            ));
        }
    };

    // Cancel any worker sessions that are currently in flight on this
    // project. Without this, a worker mid-turn would keep running,
    // advance its card on completion, and leave the project in an
    // inconsistent state (paused on the kanban; cards still moving).
    if let Ok(workers) = state.db.list_worker_sessions_by_project(&id).await {
        let mut cancelled = 0u32;
        for ws in &workers {
            if state.session_manager.is_running(&ws.id).await {
                state.session_manager.cancel(&ws.id).await;
                cancelled += 1;
            }
        }
        if cancelled > 0 {
            tracing::info!(project_id = %id, cancelled, "Cancelled in-flight workers on pause");
        }
    }

    state
        .broadcaster
        .broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "project-update".into(),
            session_id: id,
            data: serde_json::json!({ "project": &project }),
        });
    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!(project)))
}

/// POST /api/projects/:id/resume
async fn resume_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(project_id = %id, "Resuming project");
    // Clearing `pause_reason` on resume is required for the auto-pause
    // path: it only fires when `project.status == "active"`, so a stale
    // reason left behind from an earlier auto-pause would still show in
    // the UI even after the user resumed.
    let update = UpdateProject {
        status: Some("active".to_string()),
        last_accessed_at: Some(chrono::Utc::now().to_rfc3339()),
        pause_reason: Some(None),
        ..Default::default()
    };

    // Reset the consecutive-crash counter for every card the user is
    // about to retry. Without this, the next crash hits the threshold
    // immediately (the old crash events are still on disk) and the
    // user's manual retry budget collapses to one attempt.
    if let Err(e) = crate::worker::orchestrator::mark_project_resumed(&state.db, &id).await {
        tracing::warn!(project_id = %id, "Failed to mark resume sentinel: {e}");
    }

    let project = state.db.update_project(&id, update).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    match project {
        Some(p) => {
            state
                .broadcaster
                .broadcast(crate::ws::broadcaster::WsEvent {
                    event_type: "project-update".into(),
                    session_id: id,
                    data: serde_json::json!({ "project": &p }),
                });
            Ok(Json(serde_json::json!(p)))
        }
        None => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "project not found" })),
        )),
    }
}

/// GET /api/projects/:id/pending-questions -- list unresolved questions from worker sessions
async fn list_pending_questions(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(project_id = %id, "Listing pending questions");
    let worker_sessions = state
        .db
        .list_worker_sessions_by_project(&id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    let mut pending_questions = Vec::new();

    for session in &worker_sessions {
        let events = state
            .db
            .list_events_by_session(&session.id, None)
            .await
            .unwrap_or_default();

        // Collect resolved question IDs
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

        for event in &events {
            if event.kind == "question" && !resolved_ids.contains(&event.id) {
                if let Ok(data) = serde_json::from_str::<serde_json::Value>(&event.data) {
                    pending_questions.push(serde_json::json!({
                        "eventId": event.id,
                        "sessionId": session.id,
                        "ts": event.ts,
                        "questions": data.get("questions"),
                        "cardId": data.get("cardId"),
                        "cardTitle": data.get("cardTitle"),
                        "cardDescription": data.get("cardDescription"),
                        "projectId": id,
                    }));
                }
            }
        }
    }

    // Sort oldest first
    pending_questions.sort_by(|a, b| {
        let ts_a = a.get("ts").and_then(|v| v.as_i64()).unwrap_or(0);
        let ts_b = b.get("ts").and_then(|v| v.as_i64()).unwrap_or(0);
        ts_a.cmp(&ts_b)
    });

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(
        serde_json::json!({ "questions": pending_questions }),
    ))
}

/// GET /api/projects/:id/todos -- aggregate todos across every card in the
/// project, grouped by card. See `Db::list_project_todos` for the per-card
/// session-id fallback logic, which mirrors the frontend `useProjectTodos`
/// hook so the dedicated view and the in-board summary stay in sync.
async fn list_project_todos(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(project_id = %id, "Listing project todos");
    let cards = state.db.list_project_todos(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;
    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({ "cards": cards })))
}

#[derive(Deserialize)]
struct UpsertWorkflowInstructionRequest {
    workflow_id: String,
    step: String,
    /// Empty / whitespace-only text deletes the override — the absence
    /// of a row is the canonical "no additional instructions" state.
    instructions: String,
}

/// GET /api/projects/:id/workflow-instructions — list every per-step
/// override the project has set, paired with the built-in step text so
/// the UI doesn't have to look the platform defaults up separately.
async fn list_workflow_instructions(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(project_id = %id, "Listing project workflow instructions");
    let rows = state
        .db
        .list_project_workflow_instructions(&id)
        .await
        .map_err(internal_error)?;
    // Build a lookup keyed by (workflow_id, step) so the UI can match
    // overrides up against each built-in step cheaply.
    let overrides: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "workflow_id": r.workflow_id,
                "step": r.step,
                "instructions": r.instructions,
                "updated_at": r.updated_at,
            })
        })
        .collect();
    Ok::<_, RouteError>(Json(serde_json::json!({ "instructions": overrides })))
}

/// PUT /api/projects/:id/workflow-instructions — upsert a single
/// (workflow_id, step) override. Empty instructions delete the row, so
/// the same endpoint clears an override too.
async fn upsert_workflow_instruction(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<UpsertWorkflowInstructionRequest>,
) -> impl IntoResponse {
    tracing::info!(
        project_id = %id,
        workflow_id = %body.workflow_id,
        step = %body.step,
        "Upserting workflow instruction",
    );

    let workflow_id = body.workflow_id.trim();
    let step = body.step.trim();
    if workflow_id.is_empty() {
        return Err(bad_request("workflow_id is required"));
    }
    if step.is_empty() {
        return Err(bad_request("step is required"));
    }
    // Validate the (workflow, step) pair against the in-code workflow
    // registry. Without this a typo'd step would silently land in the
    // table and never reach a worker.
    let wf = workflow::workflow_by_id(workflow_id)
        .ok_or_else(|| bad_request(format!("unknown workflow id '{workflow_id}'")))?;
    if !wf.steps.iter().any(|s| s.step == step) {
        return Err(bad_request(format!(
            "workflow '{workflow_id}' has no step '{step}'"
        )));
    }
    // Don't let the user attach instructions to terminal steps — those
    // never run a worker, so the override would be silently ignored.
    if wf
        .steps
        .iter()
        .find(|s| s.step == step)
        .map(|s| s.instructions.is_empty())
        .unwrap_or(false)
    {
        return Err(bad_request(format!(
            "step '{step}' does not run a worker; cannot attach instructions"
        )));
    }

    // Ensure the project exists so we don't insert orphan rows.
    let exists = state
        .db
        .get_project(&id)
        .await
        .map_err(internal_error)?
        .is_some();
    if !exists {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "project not found" })),
        ));
    }

    let row = state
        .db
        .upsert_project_workflow_instruction(&id, workflow_id, step, &body.instructions)
        .await
        .map_err(internal_error)?;

    Ok::<_, RouteError>(Json(serde_json::json!({
        "workflow_id": workflow_id,
        "step": step,
        "instructions": row.map(|r| r.instructions).unwrap_or_default(),
    })))
}

#[cfg(test)]
mod tests {
    use super::would_create_cycle;
    use std::collections::HashMap;

    fn edges(pairs: &[(&str, &str)]) -> HashMap<String, Vec<String>> {
        let mut m: HashMap<String, Vec<String>> = HashMap::new();
        for (c, d) in pairs {
            m.entry(c.to_string()).or_default().push(d.to_string());
        }
        m
    }

    #[test]
    fn direct_self_dependency_is_a_cycle() {
        let e = edges(&[]);
        assert!(would_create_cycle(&e, "a", &["a".to_string()]));
    }

    #[test]
    fn back_edge_closes_a_cycle() {
        // Existing: b depends on a. Making a depend on b closes a->b->a.
        let e = edges(&[("b", "a")]);
        assert!(would_create_cycle(&e, "a", &["b".to_string()]));
    }

    #[test]
    fn transitive_back_edge_is_a_cycle() {
        // c->b->a already; adding a->c closes the loop.
        let e = edges(&[("c", "b"), ("b", "a")]);
        assert!(would_create_cycle(&e, "a", &["c".to_string()]));
    }

    #[test]
    fn independent_dependencies_are_fine() {
        // a and b both depend on shared leaf z — no cycle.
        let e = edges(&[("a", "z"), ("b", "z")]);
        assert!(!would_create_cycle(&e, "a", &["b".to_string()]));
    }
}
