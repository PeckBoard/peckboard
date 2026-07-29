//! `/api/plans/*` — durable plans, read-mostly.
//!
//! A plan is the Markdown design a thinking model proposes (via the
//! `propose_plan` MCP tool) for a card or a chat task. It lives in the
//! `plans` table so it survives model switches, termination, and
//! `clear_session`. This surface is what the UI reads: the 3-dots-menu
//! full-page viewer, and the picker the Document Review wizard fills its
//! `plan` source kind from.
//!
//! Reviewing a plan is not here. It used to be — per-line `plan_comments`
//! plus a `review-complete` that synthesized them into one chat message —
//! and that was a second, weaker copy of Document Review: no versions, no
//! diff, no revision history, one flat line anchor per note. A plan is now
//! reviewed as a `plan`-kind doc review like any other document, so it gets
//! passes, versions and an audit trail for free.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::get,
};
use std::sync::Arc;

use crate::auth::middleware::require_auth;
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/plans", get(get_plan_by_context))
        .route("/api/plans/{id}", get(get_plan).delete(delete_plan))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

fn err(status: StatusCode, msg: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    (
        status,
        Json(serde_json::json!({ "error": msg.to_string() })),
    )
}

#[derive(serde::Deserialize)]
struct PlanQuery {
    card_id: Option<String>,
    session_id: Option<String>,
}

/// GET /api/plans?card_id=X | ?session_id=Y → the latest plan for that
/// context, or 204 No Content when none exists (so the menu item disables).
///
/// With neither parameter it lists every plan as `{ "plans": [...] }` — the
/// shape a picker needs (the Document Review wizard's `plan` source kind
/// chooses from all plans, with no card or session to key off).
async fn get_plan_by_context(
    State(state): State<Arc<AppState>>,
    Query(q): Query<PlanQuery>,
) -> impl IntoResponse {
    let plan = if let Some(card_id) = q.card_id.as_deref() {
        state.db.get_plan_for_card(card_id).await
    } else if let Some(session_id) = q.session_id.as_deref() {
        state.db.get_plan_for_session(session_id).await
    } else {
        return match state.db.list_plans().await {
            Ok(plans) => Ok(Json(serde_json::json!({ "plans": plans })).into_response()),
            Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
        };
    };
    match plan {
        Ok(Some(p)) => Ok(Json(serde_json::json!({ "plan": p })).into_response()),
        Ok(None) => Ok(StatusCode::NO_CONTENT.into_response()),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

/// GET /api/plans/{id} → one plan.
async fn get_plan(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> impl IntoResponse {
    match state.db.get_plan(&id).await {
        Ok(Some(plan)) => Ok(Json(serde_json::json!({ "plan": plan }))),
        Ok(None) => Err(err(StatusCode::NOT_FOUND, "plan not found")),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

/// DELETE /api/plans/{id}
async fn delete_plan(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.db.delete_plan(&id).await {
        Ok(_) => Ok(StatusCode::NO_CONTENT),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}
