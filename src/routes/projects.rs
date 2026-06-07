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
use crate::db::models::{NewCard, NewProject, UpdateCard, UpdateProject};
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
}

fn default_worker_count() -> i32 {
    1
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
    // Cascade: collect worker session IDs from cards, then clean up
    let cards = state.db.list_cards_by_project(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    // Collect all worker session IDs (current and last)
    let mut session_ids: Vec<String> = Vec::new();
    for card in &cards {
        if let Some(ref sid) = card.worker_session_id {
            session_ids.push(sid.clone());
        }
        if let Some(ref sid) = card.last_worker_session_id {
            session_ids.push(sid.clone());
        }
    }
    session_ids.sort();
    session_ids.dedup();

    // Delete events then sessions for each worker session
    for sid in &session_ids {
        let _ = state.db.delete_events_by_session(sid).await;
        let _ = state.db.delete_session(sid).await;
    }

    // Delete all cards
    state.db.delete_cards_by_project(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    let deleted = state.db.delete_project(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    if !deleted {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "project not found" })),
        ));
    }

    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/projects/:id/pause
async fn pause_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(project_id = %id, "Pausing project");
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

    match project {
        Some(p) => Ok(Json(serde_json::json!(p))),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "project not found" })),
        )),
    }
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
            Json(serde_json::json!({ "error": format!("invalid priority: {}. Use GET /api/priorities for valid values.", body.priority) })),
        ));
    }

    // Hook: card.create.before — plugins can validate or modify
    let hook_result = state.plugins.dispatch(
        "card.create.before",
        serde_json::json!({
            "projectId": project_id,
            "title": body.title,
            "priority": body.priority,
        }),
    ).await;
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
            project_id,
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

    // Broadcast card creation for live kanban
    state.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
        event_type: "card-update".into(),
        session_id: card.project_id.clone(),
        data: serde_json::json!({ "card": card }),
    });

    Ok::<_, (StatusCode, Json<serde_json::Value>)>((
        StatusCode::CREATED,
        Json(serde_json::json!(card)),
    ))
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

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!(cards)))
}

/// PUT /api/projects/:id/cards/:card_id
async fn update_card(
    State(state): State<Arc<AppState>>,
    Path((_project_id, card_id)): Path<(String, String)>,
    Json(body): Json<UpdateCardRequest>,
) -> impl IntoResponse {
    tracing::info!(card_id = %card_id, "Updating card");

    // Validate priority if being updated
    if let Some(priority) = body.priority {
        if !crate::routes::misc::is_valid_priority(priority) {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid priority: {priority}. Use GET /api/priorities for valid values.") })),
            ));
        }
    }

    // Hook: card.update.before
    let hook_result = state.plugins.dispatch(
        "card.update.before",
        serde_json::json!({
            "cardId": card_id,
            "updates": serde_json::to_value(&body).unwrap_or_default(),
        }),
    ).await;
    if let crate::plugin::hooks::HookResult::Cancelled { plugin, reason } = &hook_result {
        tracing::info!(plugin = %plugin, reason = %reason, "card.update.before cancelled");
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": format!("blocked by plugin {plugin}: {reason}") })),
        ));
    }

    // Fetch existing card for edit policy checks
    let existing = state.db.get_card(&card_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    let existing = match existing {
        Some(c) => c,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "card not found" })),
            ));
        }
    };

    let is_terminal = existing.step == "done" || existing.step == "wont_do";

    // Terminal cards: only allow step changes (to reopen/move) and nothing else
    if is_terminal {
        let only_step = body.step.is_some()
            && body.title.is_none()
            && body.description.is_none()
            && body.priority.is_none()
            && body.workflow.is_none()
            && body.model.is_none()
            && body.effort.is_none()
            && body.blocked.is_none()
            && body.block_reason.is_none();
        if !only_step {
            return Err((
                StatusCode::FORBIDDEN,
                Json(
                    serde_json::json!({ "error": "card is in terminal state — only step changes allowed" }),
                ),
            ));
        }
    }

    // Reject updates to backlog-only fields (description, workflow) after leaving backlog.
    // Model, effort, title, priority, blocked, block_reason remain editable in any non-terminal state.
    if existing.step != "backlog" && !is_terminal {
        if body.workflow.is_some() || body.description.is_some() {
            return Err((
                StatusCode::FORBIDDEN,
                Json(
                    serde_json::json!({ "error": "description and workflow are locked after leaving backlog" }),
                ),
            ));
        }
    }

    let update = UpdateCard {
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
    };

    let card = state.db.update_card(&card_id, update).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    match card {
        Some(c) => {
            // Broadcast card update for live kanban
            state.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
                event_type: "card-update".into(),
                session_id: c.project_id.clone(),
                data: serde_json::json!({ "card": c }),
            });
            Ok(Json(serde_json::json!(c)))
        }
        None => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "card not found" })),
        )),
    }
}

/// DELETE /api/projects/:id/cards/:card_id
async fn delete_card(
    State(state): State<Arc<AppState>>,
    Path((_project_id, card_id)): Path<(String, String)>,
) -> impl IntoResponse {
    tracing::info!(card_id = %card_id, "Deleting card");
    // Get card before deletion for broadcast
    let card_before = state.db.get_card(&card_id).await.ok().flatten();
    let deleted = state.db.delete_card(&card_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    if !deleted {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "card not found" })),
        ));
    }

    // Broadcast card deletion for live kanban
    if let Some(card) = card_before {
        state.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "card-delete".into(),
            session_id: card.project_id.clone(),
            data: serde_json::json!({ "cardId": card_id, "projectId": card.project_id }),
        });
    }

    Ok(StatusCode::NO_CONTENT)
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
