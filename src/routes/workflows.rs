//! `/api/workflows/*` — the merged built-in + custom workflow list, and
//! CRUD for user-defined workflows.
//!
//! Built-ins live in `crate::workflow::WORKFLOWS` and are read-only; this
//! module only ever writes to the `custom_workflows` / `custom_workflow_steps`
//! tables. Every mutation reloads the in-memory registry
//! (`crate::workflow::set_custom_workflows`) so the orchestrator, the worker
//! prompt builder, and every other reader see the change immediately.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::get,
};
use std::sync::Arc;

use crate::auth::middleware::{require_admin, require_auth};
use crate::db::models::{CustomWorkflowRow, CustomWorkflowStepRow};
use crate::state::AppState;
use crate::workflow::{self, WorkflowStepDef};

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .merge(admin_router())
        .merge(user_router())
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// Custom workflows are global, unpartitioned config: a step's `instructions`
/// text is injected verbatim into the worker prompt of every project that uses
/// the workflow, in folders the editor may have no access to. Writing one is
/// therefore host-wide privilege — admin-only, same reasoning as
/// `routes/settings.rs` and `routes/folders.rs`.
///
/// Layers run outer-to-inner, so `require_admin` is appended here and
/// `require_auth` in [`router`] afterwards, which puts `AuthUser` into the
/// extensions before this middleware reads it.
fn admin_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/workflows", axum::routing::post(create_workflow))
        .route(
            "/api/workflows/{id}",
            axum::routing::put(update_workflow).delete(delete_workflow),
        )
        .route_layer(middleware::from_fn(require_admin))
}

/// Listing is readable by any authenticated user — the card and project
/// editors need the merged built-in + custom list to render a workflow picker.
fn user_router() -> Router<Arc<AppState>> {
    Router::new().route("/api/workflows", get(list_workflows))
}

fn err(status: StatusCode, msg: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    (
        status,
        Json(serde_json::json!({ "error": msg.to_string() })),
    )
}

/// Reload the in-memory custom-workflow registry from the DB. Called after
/// every mutation; also called once at startup (see `main.rs`).
pub async fn reload_registry(db: &crate::db::Db) -> anyhow::Result<()> {
    let rows = db.list_custom_workflows().await?;
    let defs = rows.into_iter().map(to_workflow_def).collect();
    workflow::set_custom_workflows(defs);
    Ok(())
}

fn to_workflow_def(w: crate::db::crud::CustomWorkflowWithSteps) -> workflow::WorkflowDef {
    workflow::WorkflowDef {
        id: w.row.id,
        name: w.row.name,
        description: w.row.description,
        priority: w.row.priority as u32,
        source: "custom",
        steps: w
            .steps
            .into_iter()
            .map(|s| WorkflowStepDef {
                step: s.step,
                instructions: s.instructions,
            })
            .collect(),
    }
}

/// GET /api/workflows — built-ins + custom, each carrying a `source` field.
async fn list_workflows() -> impl IntoResponse {
    Json(serde_json::json!({ "workflows": workflow::all_workflows() }))
}

#[derive(serde::Deserialize)]
struct StepBody {
    step: String,
    instructions: String,
}

#[derive(serde::Deserialize)]
struct CreateWorkflowBody {
    name: String,
    description: Option<String>,
    priority: Option<i32>,
    steps: Vec<StepBody>,
}

fn to_step_defs(steps: Vec<StepBody>) -> Vec<WorkflowStepDef> {
    steps
        .into_iter()
        .map(|s| WorkflowStepDef {
            step: s.step.trim().to_string(),
            instructions: s.instructions,
        })
        .collect()
}

/// POST /api/workflows — create a custom workflow.
async fn create_workflow(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateWorkflowBody>,
) -> impl IntoResponse {
    let name = body.name.trim().to_string();
    if let Err(e) = workflow::validate_workflow_name(&name, None) {
        return Err(err(StatusCode::BAD_REQUEST, e));
    }
    let steps = to_step_defs(body.steps);
    if let Err(e) = workflow::validate_workflow_steps(&steps) {
        return Err(err(StatusCode::BAD_REQUEST, e));
    }

    let now = chrono::Utc::now().to_rfc3339();
    let id = uuid::Uuid::new_v4().to_string();
    let priority = body.priority.unwrap_or(1000);

    let row = CustomWorkflowRow {
        id: id.clone(),
        name,
        description: body.description.unwrap_or_default(),
        priority,
        created_at: now.clone(),
        updated_at: now,
    };
    let step_rows: Vec<CustomWorkflowStepRow> = steps
        .iter()
        .enumerate()
        .map(|(i, s)| CustomWorkflowStepRow {
            workflow_id: id.clone(),
            position: i as i32,
            step: s.step.clone(),
            instructions: s.instructions.clone(),
        })
        .collect();

    let created = state
        .db
        .create_custom_workflow(row, step_rows)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    reload_registry(&state.db)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>((
        StatusCode::CREATED,
        Json(serde_json::json!(to_workflow_def(created))),
    ))
}

#[derive(serde::Deserialize)]
struct UpdateWorkflowBody {
    name: String,
    description: Option<String>,
    priority: Option<i32>,
    steps: Vec<StepBody>,
}

/// PUT /api/workflows/{id} — replace a custom workflow's metadata + steps.
/// Rejects built-in ids (404, since built-ins aren't in the custom table).
async fn update_workflow(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<UpdateWorkflowBody>,
) -> impl IntoResponse {
    let existing = state
        .db
        .get_custom_workflow(&id)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if existing.is_none() {
        return Err(err(
            StatusCode::NOT_FOUND,
            "custom workflow not found (built-in workflows can't be edited)",
        ));
    }

    let name = body.name.trim().to_string();
    if let Err(e) = workflow::validate_workflow_name(&name, Some(&id)) {
        return Err(err(StatusCode::BAD_REQUEST, e));
    }
    let steps = to_step_defs(body.steps);
    if let Err(e) = workflow::validate_workflow_steps(&steps) {
        return Err(err(StatusCode::BAD_REQUEST, e));
    }

    // Editing steps is not free: a card sitting on a step this edit removes
    // (or renames) would, on its next `complete_step`, find no matching step
    // in the workflow and get stamped straight to `done` — silently skipping
    // every gate in between. Block the edit instead, the same way
    // `delete_workflow` blocks on references. Only steps the OLD workflow
    // actually declares are considered, so a card already sitting on an
    // unknown step (bad data) can't wedge every future edit.
    let old_steps: Vec<String> = existing
        .as_ref()
        .map(|w| w.steps.iter().map(|s| s.step.clone()).collect())
        .unwrap_or_default();
    let new_step_names: std::collections::HashSet<&str> =
        steps.iter().map(|s| s.step.as_str()).collect();
    let in_flight = state
        .db
        .custom_workflow_in_flight_cards(&id)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let blocking: Vec<(String, String, String)> = in_flight
        .into_iter()
        .filter(|(_, _, step)| {
            old_steps.iter().any(|s| s == step) && !new_step_names.contains(step.as_str())
        })
        .collect();
    if !blocking.is_empty() {
        let mut missing: Vec<&str> = blocking.iter().map(|(_, _, s)| s.as_str()).collect();
        missing.sort_unstable();
        missing.dedup();
        let card_count = blocking.len();
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!(
                    "{card_count} card(s) are currently on step(s) this edit removes: {}. \
                     Move those cards first, or keep the step(s).",
                    missing.join(", ")
                ),
                "steps": missing,
                "cards": blocking.iter().map(|(cid, title, step)| serde_json::json!({
                    "id": cid,
                    "title": title,
                    "step": step,
                })).collect::<Vec<_>>(),
                "card_count": card_count,
            })),
        ));
    }

    let now = chrono::Utc::now().to_rfc3339();
    let step_rows: Vec<CustomWorkflowStepRow> = steps
        .iter()
        .enumerate()
        .map(|(i, s)| CustomWorkflowStepRow {
            workflow_id: id.clone(),
            position: i as i32,
            step: s.step.clone(),
            instructions: s.instructions.clone(),
        })
        .collect();

    let updated = state
        .db
        .update_custom_workflow(
            &id,
            name,
            body.description.unwrap_or_default(),
            body.priority.unwrap_or(1000),
            step_rows,
            now,
        )
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    let updated = match updated {
        Some(u) => u,
        None => return Err(err(StatusCode::NOT_FOUND, "custom workflow not found")),
    };

    reload_registry(&state.db)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!(to_workflow_def(
        updated
    ))))
}

/// DELETE /api/workflows/{id} — delete a custom workflow. 404 for
/// built-ins or unknown ids; 409 (with the referencing projects and card
/// count) if any project or card still names this workflow — no cascade.
async fn delete_workflow(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let existing = state
        .db
        .get_custom_workflow(&id)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if existing.is_none() {
        return Err(err(
            StatusCode::NOT_FOUND,
            "custom workflow not found (built-in workflows can't be deleted)",
        ));
    }

    let (references, card_count) = state
        .db
        .custom_workflow_references(&id)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if !references.is_empty() || card_count > 0 {
        let project_names: Vec<&str> = references.iter().map(|r| r.project_name.as_str()).collect();
        let mut msg = String::from("workflow is still in use");
        if !project_names.is_empty() {
            msg.push_str(&format!(" by project(s): {}", project_names.join(", ")));
        }
        if card_count > 0 {
            msg.push_str(&format!(" and {card_count} card(s)"));
        }
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": msg,
                "projects": references.iter().map(|r| serde_json::json!({
                    "id": r.project_id,
                    "name": r.project_name,
                })).collect::<Vec<_>>(),
                "card_count": card_count,
            })),
        ));
    }

    state
        .db
        .delete_custom_workflow(&id)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    reload_registry(&state.db)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(StatusCode::NO_CONTENT)
}
