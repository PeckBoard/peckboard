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
use crate::db::models::{Card, NewCard, NewProject, UpdateCard, UpdateProject};
use crate::state::AppState;

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
    default_workflow: Option<String>,
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
    default_workflow: Option<Option<String>>,
    model: Option<Option<String>>,
    effort: Option<Option<String>>,
    parallel_instructions: Option<bool>,
    auto_notify_changes: Option<bool>,
    worker_communication: Option<bool>,
}

#[derive(Deserialize)]
struct CreateCardRequest {
    title: String,
    description: String,
    step: String,
    priority: i32,
    workflow: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    /// Ids of cards this card depends on (must be `done` before a worker
    /// will pick this card up).
    depends_on: Option<Vec<String>>,
}

#[derive(Deserialize, serde::Serialize)]
struct UpdateCardRequest {
    title: Option<String>,
    description: Option<String>,
    step: Option<String>,
    priority: Option<i32>,
    workflow: Option<Option<String>>,
    model: Option<Option<String>>,
    effort: Option<Option<String>>,
    worker_session_id: Option<Option<String>>,
    last_worker_session_id: Option<Option<String>>,
    handoff_context: Option<Option<String>>,
    blocked: Option<bool>,
    block_reason: Option<Option<String>>,
    /// When present, replaces the card's full dependency set.
    depends_on: Option<Vec<String>>,
}

// ── Card dependency helpers ─────────────────────────────────────────

type RouteError = (StatusCode, Json<serde_json::Value>);

fn bad_request(msg: impl Into<String>) -> RouteError {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": msg.into() })),
    )
}

fn internal_error(e: impl std::fmt::Display) -> RouteError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": e.to_string() })),
    )
}

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
async fn apply_dependencies(
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
async fn card_json_with_deps(state: &AppState, card: &Card) -> serde_json::Value {
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
            post(create_card).get(list_cards),
        )
        .route(
            "/api/projects/{id}/cards/{card_id}",
            put(update_card).delete(delete_card),
        )
        .route(
            "/api/projects/{id}/cards/{card_id}/stop",
            post(stop_card_worker),
        )
        .route(
            "/api/projects/{id}/cards/{card_id}/restart",
            post(restart_card_worker),
        )
        .route(
            "/api/projects/{id}/cards/{card_id}/cancel-wont-do",
            post(cancel_card_wont_do),
        )
        .route(
            "/api/projects/{id}/cards/{card_id}/reports",
            get(list_card_reports),
        )
        .route(
            "/api/projects/{id}/pending-questions",
            get(list_pending_questions),
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
            default_workflow: body.default_workflow,
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
    let update = UpdateProject {
        name: body.name,
        context: body.context,
        worker_count: body.worker_count,
        status: body.status,
        default_workflow: body.default_workflow,
        model: body.model,
        effort: body.effort,
        parallel_instructions: body.parallel_instructions,
        auto_notify_changes: body.auto_notify_changes,
        worker_communication: body.worker_communication,
        last_accessed_at: Some(chrono::Utc::now().to_rfc3339()),
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

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!(project)))
}

/// POST /api/projects/:id/resume
async fn resume_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(project_id = %id, "Resuming project");
    let update = UpdateProject {
        status: Some("active".to_string()),
        last_accessed_at: Some(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
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

// ── Card handlers ───────────────────────────────────────────────────

/// POST /api/projects/:id/cards
async fn create_card(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<String>,
    Json(body): Json<CreateCardRequest>,
) -> impl IntoResponse {
    tracing::info!(project_id = %project_id, title = %body.title, "Creating card");

    // Validate priority
    if !crate::routes::misc::is_valid_priority(body.priority) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(
                serde_json::json!({ "error": format!("invalid priority: {}. Use GET /api/priorities for valid values.", body.priority) }),
            ),
        ));
    }

    // Hook: card.create.before — plugins can validate or modify
    let hook_result = state
        .plugins
        .dispatch(
            "card.create.before",
            serde_json::json!({
                "projectId": project_id,
                "title": body.title,
                "priority": body.priority,
            }),
        )
        .await;
    if let crate::plugin::hooks::HookResult::Cancelled { plugin, reason } = &hook_result {
        tracing::info!(plugin = %plugin, reason = %reason, "card.create.before cancelled");
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": format!("blocked by plugin {plugin}: {reason}") })),
        ));
    }

    // Verify project exists
    let project = state.db.get_project(&project_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    if project.is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "project not found" })),
        ));
    }

    let now = chrono::Utc::now().to_rfc3339();
    let id = uuid::Uuid::new_v4().to_string();

    let card = state
        .db
        .create_card(NewCard {
            id,
            project_id: project_id.clone(),
            title: body.title,
            description: body.description,
            step: body.step,
            priority: body.priority,
            workflow: body.workflow,
            model: body.model,
            effort: body.effort,
            created_at: now.clone(),
            updated_at: now,
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    // Persist dependencies if requested. On validation failure roll the
    // card back so we don't leave a half-created card behind.
    if let Some(depends_on) = body.depends_on {
        if let Err(err) = apply_dependencies(&state, &project_id, &card.id, depends_on).await {
            let _ = state.db.delete_card(&card.id).await;
            return Err(err);
        }
    }

    let card_value = card_json_with_deps(&state, &card).await;

    // Broadcast card creation for live kanban
    state
        .broadcaster
        .broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "card-update".into(),
            session_id: card.project_id.clone(),
            data: serde_json::json!({ "card": card_value }),
        });

    Ok::<_, (StatusCode, Json<serde_json::Value>)>((StatusCode::CREATED, Json(card_value)))
}

/// GET /api/projects/:id/cards
async fn list_cards(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(project_id = %project_id, "Listing cards");
    let cards = state
        .db
        .list_cards_by_project(&project_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    // Attach each card's dependency ids from a single project-wide query.
    let edges = state
        .db
        .list_dependencies_by_project(&project_id)
        .await
        .unwrap_or_default();
    let mut deps_by_card: std::collections::HashMap<&str, Vec<&str>> =
        std::collections::HashMap::new();
    for (card_id, dep_id) in &edges {
        deps_by_card
            .entry(card_id.as_str())
            .or_default()
            .push(dep_id.as_str());
    }

    let items: Vec<serde_json::Value> = cards
        .iter()
        .map(|c| {
            let deps = deps_by_card.get(c.id.as_str()).cloned().unwrap_or_default();
            let mut value = serde_json::to_value(c).unwrap_or_else(|_| serde_json::json!({}));
            if let Some(obj) = value.as_object_mut() {
                obj.insert("depends_on".into(), serde_json::json!(deps));
            }
            value
        })
        .collect();

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!(items)))
}

/// PUT /api/projects/:id/cards/:card_id
async fn update_card(
    State(state): State<Arc<AppState>>,
    Path((_project_id, card_id)): Path<(String, String)>,
    Json(mut body): Json<UpdateCardRequest>,
) -> impl IntoResponse {
    tracing::info!(card_id = %card_id, "Updating card");

    // Validate priority if being updated
    if let Some(priority) = body.priority {
        if !crate::routes::misc::is_valid_priority(priority) {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(
                    serde_json::json!({ "error": format!("invalid priority: {priority}. Use GET /api/priorities for valid values.") }),
                ),
            ));
        }
    }

    // Hook: card.update.before
    let hook_result = state
        .plugins
        .dispatch(
            "card.update.before",
            serde_json::json!({
                "cardId": card_id,
                "updates": serde_json::to_value(&body).unwrap_or_default(),
            }),
        )
        .await;
    if let crate::plugin::hooks::HookResult::Cancelled { plugin, reason } = &hook_result {
        tracing::info!(plugin = %plugin, reason = %reason, "card.update.before cancelled");
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": format!("blocked by plugin {plugin}: {reason}") })),
        ));
    }

    // Pull `depends_on` out before the atomic update closure so the
    // body fields it captures don't include the dep set. Replacing
    // dependencies is a separate table write that needs to validate
    // (unknown dep / cycle) against the project's full card graph; we
    // do it after the atomic card update succeeds so a failed card
    // write doesn't leave dependencies in an inconsistent state.
    let depends_on = body.depends_on.take();

    // Atomic validate + update under the DB connection mutex. Holding
    // the mutex across the read-validate-write closure prevents two
    // concurrent transitions from both seeing the same pre-state and
    // both applying their write (e.g. two `complete_step` calls racing
    // and producing inconsistent step values).
    let depends_on_present = depends_on.is_some();
    let card = state
        .db
        .update_card_atomic(&card_id, move |existing| {
            let is_terminal = existing.step == "done" || existing.step == "wont_do";

            // Terminal cards: only step changes allowed (to reopen / move).
            // depends_on edits are also blocked in terminal states.
            if is_terminal {
                let only_step = body.step.is_some()
                    && body.title.is_none()
                    && body.description.is_none()
                    && body.priority.is_none()
                    && body.workflow.is_none()
                    && body.model.is_none()
                    && body.effort.is_none()
                    && body.blocked.is_none()
                    && body.block_reason.is_none()
                    && !depends_on_present;
                if !only_step {
                    anyhow::bail!(
                        "card-update-policy: card is in terminal state — only step changes allowed"
                    );
                }
            }

            // description/workflow are locked once a card leaves backlog.
            // model, effort, title, priority, blocked, block_reason stay
            // editable in any non-terminal state.
            if existing.step != "backlog"
                && !is_terminal
                && (body.workflow.is_some() || body.description.is_some())
            {
                anyhow::bail!(
                    "card-update-policy: description and workflow are locked after leaving backlog"
                );
            }

            Ok(UpdateCard {
                title: body.title,
                description: body.description,
                step: body.step,
                priority: body.priority,
                workflow: body.workflow,
                model: body.model,
                effort: body.effort,
                worker_session_id: body.worker_session_id,
                last_worker_session_id: body.last_worker_session_id,
                handoff_context: body.handoff_context,
                blocked: body.blocked,
                block_reason: body.block_reason,
                updated_at: Some(chrono::Utc::now().to_rfc3339()),
            })
        })
        .await;

    let card = match card {
        Ok(c) => c,
        Err(e) => {
            let msg = e.to_string();
            // Validation rejections from the closure are user-correctable
            // (terminal-state or backlog-locked policy); everything else
            // is a server-side error.
            let status = if msg.starts_with("card-update-policy:") {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            return Err((
                status,
                Json(serde_json::json!({
                    "error": msg.trim_start_matches("card-update-policy: ").to_string()
                })),
            ));
        }
    };

    let Some(c) = card else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "card not found" })),
        ));
    };

    // Apply dependency replacements after the card row update has
    // succeeded. `apply_dependencies` validates unknown ids / cycles
    // and returns a 4xx via the response type if rejected.
    if let Some(deps) = depends_on {
        apply_dependencies(&state, &c.project_id, &card_id, deps).await?;
    }

    let card_value = card_json_with_deps(&state, &c).await;
    state
        .broadcaster
        .broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "card-update".into(),
            session_id: c.project_id.clone(),
            data: serde_json::json!({ "card": card_value }),
        });
    Ok(Json(card_value))
}

/// DELETE /api/projects/:id/cards/:card_id
async fn delete_card(
    State(state): State<Arc<AppState>>,
    Path((_project_id, card_id)): Path<(String, String)>,
) -> impl IntoResponse {
    tracing::info!(card_id = %card_id, "Deleting card");
    // Grab the project_id before we delete so we can still broadcast a
    // card-delete event with it.
    let project_id = state
        .db
        .get_card(&card_id)
        .await
        .ok()
        .flatten()
        .map(|c| c.project_id);

    // Atomic cascade. Replaces a sequence of separate awaits with
    // `let _ = …` that silently swallowed errors and could leave
    // orphaned events/sessions when a step failed.
    let report = state.db.delete_card_cascade(&card_id).await.map_err(|e| {
        let msg = e.to_string();
        let status = if msg.contains("not found") {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        (status, Json(serde_json::json!({ "error": msg })))
    })?;
    tracing::info!(
        card_id = %card_id,
        sessions = report.sessions_deleted,
        events = report.events_deleted,
        "Card cascade-deleted"
    );

    if let Some(pid) = project_id {
        state
            .broadcaster
            .broadcast(crate::ws::broadcaster::WsEvent {
                event_type: "card-delete".into(),
                session_id: pid.clone(),
                data: serde_json::json!({ "cardId": card_id, "projectId": pid }),
            });
    }

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(StatusCode::NO_CONTENT)
}

/// POST /api/projects/:id/cards/:card_id/stop -- stop the card's active worker
async fn stop_card_worker(
    State(state): State<Arc<AppState>>,
    Path((_project_id, card_id)): Path<(String, String)>,
) -> impl IntoResponse {
    tracing::info!(card_id = %card_id, "Stopping card worker");
    let card = state.db.get_card(&card_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;
    let card = card.ok_or((
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "card not found" })),
    ))?;

    if let Some(session_id) = &card.worker_session_id {
        state.session_manager.cancel(session_id).await;
        state
            .db
            .update_card(
                &card_id,
                crate::db::models::UpdateCard {
                    worker_session_id: Some(None),
                    last_worker_session_id: Some(Some(session_id.clone())),
                    ..Default::default()
                },
            )
            .await
            .ok();
    }

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({ "ok": true })))
}

/// POST /api/projects/:id/cards/:card_id/restart -- restart the card's worker
async fn restart_card_worker(
    State(state): State<Arc<AppState>>,
    Path((_project_id, card_id)): Path<(String, String)>,
) -> impl IntoResponse {
    tracing::info!(card_id = %card_id, "Restarting card worker");
    let card = state.db.get_card(&card_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;
    let card = card.ok_or((
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "card not found" })),
    ))?;

    // Stop existing worker if running
    if let Some(session_id) = &card.worker_session_id {
        state.session_manager.cancel(session_id).await;
        state
            .db
            .update_card(
                &card_id,
                crate::db::models::UpdateCard {
                    worker_session_id: Some(None),
                    last_worker_session_id: Some(Some(session_id.clone())),
                    ..Default::default()
                },
            )
            .await
            .ok();
    }

    // Unblock if blocked
    if card.blocked {
        state
            .db
            .update_card(
                &card_id,
                crate::db::models::UpdateCard {
                    blocked: Some(false),
                    block_reason: Some(None),
                    ..Default::default()
                },
            )
            .await
            .ok();
    }

    // The watchdog/orchestrator will pick up the unassigned card on next cycle
    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({ "ok": true })))
}

/// POST /api/projects/:id/cards/:card_id/cancel-wont-do -- cancel worker and mark card as wont_do
async fn cancel_card_wont_do(
    State(state): State<Arc<AppState>>,
    Path((_project_id, card_id)): Path<(String, String)>,
) -> impl IntoResponse {
    tracing::info!(card_id = %card_id, "Cancelling card as wont_do");
    let card = state.db.get_card(&card_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;
    let card = card.ok_or((
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "card not found" })),
    ))?;

    // Stop existing worker
    if let Some(session_id) = &card.worker_session_id {
        state.session_manager.cancel(session_id).await;
    }

    // Move card to wont_do
    state
        .db
        .update_card(
            &card_id,
            crate::db::models::UpdateCard {
                step: Some("wont_do".into()),
                worker_session_id: Some(None),
                last_worker_session_id: card.worker_session_id.map(Some),
                blocked: Some(false),
                block_reason: Some(None),
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

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({ "ok": true })))
}

/// GET /api/projects/:id/cards/:card_id/reports -- list reports written by this card's worker
async fn list_card_reports(
    State(_state): State<Arc<AppState>>,
    Path((_project_id, card_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let data_dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".peckboard");
    let reports_dir = data_dir.join("reports");

    let mut reports = Vec::new();
    if reports_dir.exists() {
        if let Ok(folders) = std::fs::read_dir(&reports_dir) {
            for folder_entry in folders.flatten() {
                let folder_name = folder_entry.file_name().to_string_lossy().to_string();
                if let Ok(files) = std::fs::read_dir(folder_entry.path()) {
                    for file_entry in files.flatten() {
                        let file_name = file_entry.file_name().to_string_lossy().to_string();
                        if !file_name.ends_with(".md") {
                            continue;
                        }

                        if let Ok(content) = std::fs::read_to_string(file_entry.path()) {
                            if !content.starts_with("---") {
                                continue;
                            }
                            let fm = content.splitn(3, "---").nth(1).unwrap_or("");
                            let mut title = file_name.clone();
                            let mut report_card_id = None;
                            let mut date = String::new();

                            for line in fm.lines() {
                                if let Some(v) = line.strip_prefix("title: ") {
                                    title = v.trim_matches('"').to_string();
                                }
                                if let Some(v) = line.strip_prefix("cardId: ") {
                                    report_card_id = Some(v.trim_matches('"').to_string());
                                }
                                if let Some(v) = line.strip_prefix("date: ") {
                                    date = v.trim_matches('"').to_string();
                                }
                            }

                            if report_card_id.as_deref() == Some(&card_id) {
                                reports.push(serde_json::json!({
                                    "folder": folder_name,
                                    "file": file_name,
                                    "title": title,
                                    "date": date,
                                }));
                            }
                        }
                    }
                }
            }
        }
    }

    Json(serde_json::json!({ "reports": reports }))
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
