//! `/api/plans/*` — durable plans + per-line human review comments.
//!
//! A plan is the Markdown design a thinking model proposes (via the
//! `propose_plan` MCP tool) for a card or a chat task. It lives in the
//! `plans` table so it survives model switches, termination, and
//! `clear_session`. This surface is read-mostly for the UI (the 3-dots-menu
//! full-page viewer) plus the per-line comment flow: the human annotates the
//! proposed plan, then "review complete" synthesizes those comments into a
//! message the caller posts back to the *same* session so it revises the
//! plan with full context.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{get, post},
};
use std::sync::Arc;

use crate::auth::middleware::require_auth;
use crate::state::AppState;

const MAX_COMMENT_LEN: usize = 10_000;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/plans", get(get_plan_by_context))
        .route("/api/plans/{id}", get(get_plan).delete(delete_plan))
        .route(
            "/api/plans/{id}/comments",
            get(list_comments).post(add_comment),
        )
        .route(
            "/api/plans/{id}/comments/{comment_id}",
            axum::routing::delete(delete_comment),
        )
        .route("/api/plans/{id}/review-complete", post(review_complete))
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
async fn get_plan_by_context(
    State(state): State<Arc<AppState>>,
    Query(q): Query<PlanQuery>,
) -> impl IntoResponse {
    let plan = if let Some(card_id) = q.card_id.as_deref() {
        state.db.get_plan_for_card(card_id).await
    } else if let Some(session_id) = q.session_id.as_deref() {
        state.db.get_plan_for_session(session_id).await
    } else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "provide card_id or session_id",
        ));
    };
    match plan {
        Ok(Some(p)) => Ok(Json(serde_json::json!({ "plan": p })).into_response()),
        Ok(None) => Ok(StatusCode::NO_CONTENT.into_response()),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

/// GET /api/plans/{id} → the plan plus its open comments.
async fn get_plan(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> impl IntoResponse {
    let plan = match state.db.get_plan(&id).await {
        Ok(Some(p)) => p,
        Ok(None) => return Err(err(StatusCode::NOT_FOUND, "plan not found")),
        Err(e) => return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    };
    let comments = state
        .db
        .list_plan_comments(&id, false)
        .await
        .unwrap_or_default();
    Ok(Json(
        serde_json::json!({ "plan": plan, "comments": comments }),
    ))
}

/// DELETE /api/plans/{id} → remove the plan and its comments.
async fn delete_plan(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.db.delete_plan(&id).await {
        Ok(_) => Ok(StatusCode::NO_CONTENT),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

/// GET /api/plans/{id}/comments → open comments, ascending by line anchor.
async fn list_comments(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.db.list_plan_comments(&id, false).await {
        Ok(comments) => Ok(Json(serde_json::json!({ "comments": comments }))),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

#[derive(serde::Deserialize)]
struct AddCommentBody {
    anchor: i32,
    body: String,
}

/// POST /api/plans/{id}/comments `{anchor, body}` → 201 with the comment.
async fn add_comment(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<AddCommentBody>,
) -> impl IntoResponse {
    let body = req.body.trim();
    if body.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "comment body is empty"));
    }
    if body.len() > MAX_COMMENT_LEN {
        return Err(err(StatusCode::BAD_REQUEST, "comment too long"));
    }
    if state.db.get_plan(&id).await.ok().flatten().is_none() {
        return Err(err(StatusCode::NOT_FOUND, "plan not found"));
    }
    match state
        .db
        .add_plan_comment(&id, req.anchor.max(0), body)
        .await
    {
        Ok(c) => Ok((StatusCode::CREATED, Json(serde_json::json!(c)))),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

/// DELETE /api/plans/{id}/comments/{comment_id}
async fn delete_comment(
    State(state): State<Arc<AppState>>,
    Path((_id, comment_id)): Path<(String, String)>,
) -> impl IntoResponse {
    match state.db.delete_plan_comment(&comment_id).await {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

/// POST /api/plans/{id}/review-complete → resolve open comments, flip the
/// plan to `revising`, and return the synthesized revision request + the
/// creator `session_id`. The caller posts `message` to
/// `/api/sessions/{session_id}/message` so the SAME session revises the plan
/// (it has the full context) and calls `propose_plan` again.
async fn review_complete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let plan = match state.db.get_plan(&id).await {
        Ok(Some(p)) => p,
        Ok(None) => return Err(err(StatusCode::NOT_FOUND, "plan not found")),
        Err(e) => return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    };
    let comments = state
        .db
        .list_plan_comments(&id, false)
        .await
        .unwrap_or_default();
    if comments.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "no open comments to apply"));
    }

    let mut message = String::from(
        "I reviewed the plan and left line comments. Please revise the saved plan to address \
         each of them, then call `propose_plan` again with the updated Markdown:\n\n",
    );
    for c in &comments {
        message.push_str(&format!("- [line {}] {}\n", c.anchor, c.body));
    }

    state.db.resolve_plan_comments(&id).await.ok();
    state.db.set_plan_status(&id, "revising").await.ok();

    Ok(Json(serde_json::json!({
        "status": "ok",
        "session_id": plan.session_id,
        "message": message,
    })))
}
