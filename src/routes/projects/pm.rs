//! PM decision-log HTTP routes — the API surface the PM Q&A form uses.
//! Listing is read-only; the two mutations are the USER-only seams for
//! changing the log: answering a pending question routes through
//! [`deliver_pm_user_answer`] (the same feedback path the escalation flow
//! defined), and editing an answered decision routes through
//! [`Db::supersede_decision`]. Workers / MCP callers never reach these —
//! they are gated behind the app's user auth middleware.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use super::{RouteError, bad_request, internal_error};
use crate::db::models::PmDecision;
use crate::service::mcp_server::{AppExpertDispatcher, ExpertDispatcher};
use crate::service::pm_expert::{deliver_pm_user_answer, export_pm_decisions};
use crate::state::AppState;
use crate::ws::broadcaster::WsEvent;

fn not_found(msg: impl Into<String>) -> RouteError {
    (StatusCode::NOT_FOUND, Json(json!({ "error": msg.into() })))
}

fn conflict(msg: impl Into<String>) -> RouteError {
    (StatusCode::CONFLICT, Json(json!({ "error": msg.into() })))
}

fn decision_json(d: &PmDecision) -> serde_json::Value {
    json!({
        "id": d.id,
        "question": d.question,
        "answer": d.answer,
        "status": d.status,
        "decided_at": d.answered_at,
        "asked_by_session_id": d.asked_by_session_id,
        "asked_at": d.created_at,
    })
}

async fn require_project(state: &AppState, id: &str) -> Result<(), RouteError> {
    let exists = state
        .db
        .get_project(id)
        .await
        .map_err(internal_error)?
        .is_some();
    if exists {
        Ok(())
    } else {
        Err(not_found("project not found"))
    }
}

/// Load a decision row, 404ing when it doesn't exist or belongs to another
/// project — cross-project ids must be indistinguishable from missing ones.
async fn require_decision_in_project(
    state: &AppState,
    project_id: &str,
    decision_id: &str,
) -> Result<PmDecision, RouteError> {
    let decision = state
        .db
        .get_pm_decision(decision_id)
        .await
        .map_err(internal_error)?;
    match decision {
        Some(d) if d.project_id == project_id => Ok(d),
        _ => Err(not_found("pm decision not found")),
    }
}

/// Live-update broadcast after every decision-log mutation, mirroring the
/// `repeating-task-changed` / `project-update` pattern the frontend store
/// already consumes from.
fn broadcast_pm_change(state: &AppState, project_id: &str, action: &str, pending_count: i64) {
    state.broadcaster.broadcast(WsEvent {
        event_type: "pm-decisions-changed".into(),
        session_id: project_id.to_string(),
        data: json!({
            "projectId": project_id,
            "action": action,
            "pending_count": pending_count,
        }),
    });
}

/// GET /api/projects/:id/pm/decisions — current answered decisions
/// (superseded rows excluded) plus the pending count the UI flags on.
pub(super) async fn list_pm_decisions(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    require_project(&state, &id).await?;
    let decisions = state
        .db
        .list_answered_pm_decisions(&id)
        .await
        .map_err(internal_error)?;
    let pending_count = state
        .db
        .pending_pm_decision_count(&id)
        .await
        .map_err(internal_error)?;
    Ok::<_, RouteError>(Json(json!({
        "decisions": decisions.iter().map(decision_json).collect::<Vec<_>>(),
        "pending_count": pending_count,
    })))
}

/// GET /api/projects/:id/pm/questions — questions awaiting a user answer.
pub(super) async fn list_pm_questions(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    require_project(&state, &id).await?;
    let pending = state
        .db
        .list_pending_pm_decisions(&id)
        .await
        .map_err(internal_error)?;
    let questions: Vec<serde_json::Value> = pending
        .iter()
        .map(|q| {
            json!({
                "id": q.id,
                "question": q.question,
                "asked_by_session_id": q.asked_by_session_id,
                "asked_at": q.created_at,
            })
        })
        .collect();
    Ok::<_, RouteError>(Json(json!({ "questions": questions })))
}

#[derive(Deserialize)]
pub(super) struct AnswerQuestionRequest {
    answer: String,
}

/// POST /api/projects/:id/pm/questions/:qid/answer — the ONLY path that
/// converts a pending question into a decision. Routes through
/// [`deliver_pm_user_answer`]: marks the row answered, grants the one-shot
/// supersession authorization, delivers the answer into the PM expert
/// session as an express user decision, and regenerates the export.
pub(super) async fn answer_pm_question(
    State(state): State<Arc<AppState>>,
    Path((id, qid)): Path<(String, String)>,
    Json(body): Json<AnswerQuestionRequest>,
) -> impl IntoResponse {
    let answer = body.answer.trim();
    if answer.is_empty() {
        return Err(bad_request("answer is required"));
    }
    require_project(&state, &id).await?;
    let question = require_decision_in_project(&state, &id, &qid).await?;
    if question.status != "pending" {
        return Err(conflict(format!(
            "pm question is '{}', not pending — an answered question cannot be \
             answered again; edit the decision instead",
            question.status
        )));
    }

    tracing::info!(project_id = %id, question_id = %qid, "Answering PM question");

    let dispatcher: Arc<dyn ExpertDispatcher> = Arc::new(AppExpertDispatcher::new(state.clone()));
    let answered = deliver_pm_user_answer(
        &state.db,
        &state.broadcaster,
        &state.config.data_dir,
        Some(&dispatcher),
        &state.pm_authorizations,
        &qid,
        answer,
    )
    .await
    .map_err(internal_error)?;

    let pending_count = state
        .db
        .pending_pm_decision_count(&id)
        .await
        .map_err(internal_error)?;
    broadcast_pm_change(&state, &id, "answered", pending_count);

    Ok::<_, RouteError>(Json(json!({
        "decision": decision_json(&answered),
        "pending_count": pending_count,
    })))
}

#[derive(Deserialize)]
pub(super) struct UpdateDecisionRequest {
    question: Option<String>,
    answer: String,
}

/// PUT /api/projects/:id/pm/decisions/:did — the USER expressly edits an
/// answered decision. The ONLY way an existing decision changes: supersedes
/// the old row via [`Db::supersede_decision`] and regenerates the export.
pub(super) async fn update_pm_decision(
    State(state): State<Arc<AppState>>,
    Path((id, did)): Path<(String, String)>,
    Json(body): Json<UpdateDecisionRequest>,
) -> impl IntoResponse {
    let answer = body.answer.trim();
    if answer.is_empty() {
        return Err(bad_request("answer is required"));
    }
    require_project(&state, &id).await?;
    let old = require_decision_in_project(&state, &id, &did).await?;
    match old.status.as_str() {
        "answered" => {}
        "pending" => {
            return Err(conflict(
                "pm decision is still pending — answer it via the answer endpoint instead",
            ));
        }
        other => {
            return Err(conflict(format!(
                "pm decision is '{other}' and can no longer be edited; \
                 edit its replacement instead"
            )));
        }
    }

    let question = match body.question.as_deref().map(str::trim) {
        Some(q) if !q.is_empty() => q.to_string(),
        _ => old.question.clone(),
    };

    tracing::info!(project_id = %id, decision_id = %did, "Superseding PM decision (user edit)");

    let superseding = state
        .db
        .supersede_decision(&did, &question, answer)
        .await
        .map_err(internal_error)?;

    if let Err(e) = export_pm_decisions(&state.db, &state.config.data_dir, &id).await {
        tracing::warn!(project_id = %id, "failed to re-export PM decisions: {e}");
    }

    let pending_count = state
        .db
        .pending_pm_decision_count(&id)
        .await
        .map_err(internal_error)?;
    broadcast_pm_change(&state, &id, "superseded", pending_count);

    Ok::<_, RouteError>(Json(json!({
        "decision": decision_json(&superseding),
        "superseded_decision_id": did,
    })))
}
