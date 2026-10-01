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
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::get,
};
use std::sync::Arc;

use crate::auth::middleware::{AuthUser, require_auth};
use crate::db::models::Plan;
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
/// Whether the caller may see / delete `plan`. A plan inherits its creator
/// session's access rule ([`crate::auth::access::may_access_session`]): a
/// board-attached plan is shared like the board, a chat plan is visible only
/// to the creator session's owner. Admins see everything.
async fn may_access_plan(state: &AppState, user: &AuthUser, plan: &Plan) -> bool {
    if user.is_admin() || plan.project_id.is_some() {
        return true;
    }
    let Ok(Some(session)) = state.db.get_session(&plan.session_id).await else {
        return false;
    };
    crate::auth::access::may_access_session(
        false,
        &user.user_id,
        session.user_id.as_deref(),
        session.project_id.as_deref(),
    )
}

async fn get_plan_by_context(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Query(q): Query<PlanQuery>,
) -> impl IntoResponse {
    let plan = if let Some(card_id) = q.card_id.as_deref() {
        state.db.get_plan_for_card(card_id).await
    } else if let Some(session_id) = q.session_id.as_deref() {
        state.db.get_plan_for_session(session_id).await
    } else {
        let plans = match state.db.list_plans().await {
            Ok(plans) => plans,
            Err(e) => return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
        };
        let mut visible = Vec::with_capacity(plans.len());
        for p in plans {
            if may_access_plan(&state, &user, &p).await {
                visible.push(p);
            }
        }
        return Ok(Json(serde_json::json!({ "plans": visible })).into_response());
    };
    match plan {
        Ok(Some(p)) if may_access_plan(&state, &user, &p).await => {
            Ok(Json(serde_json::json!({ "plan": p })).into_response())
        }
        Ok(_) => Ok(StatusCode::NO_CONTENT.into_response()),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

/// The plan, if it exists and the caller may access it — 404 otherwise so a
/// plan id can't be used as an existence oracle.
async fn authorized_plan(
    state: &AppState,
    user: &AuthUser,
    id: &str,
) -> Result<Plan, (StatusCode, Json<serde_json::Value>)> {
    match state.db.get_plan(id).await {
        Ok(Some(plan)) if may_access_plan(state, user, &plan).await => Ok(plan),
        Ok(_) => Err(err(StatusCode::NOT_FOUND, "plan not found")),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

/// GET /api/plans/{id} → one plan.
async fn get_plan(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let plan = authorized_plan(&state, &user, &id).await?;
    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!({ "plan": plan })))
}

/// DELETE /api/plans/{id}
async fn delete_plan(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    authorized_plan(&state, &user, &id).await?;
    match state.db.delete_plan(&id).await {
        Ok(_) => Ok(StatusCode::NO_CONTENT),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::middleware::tests::{seed_authenticated_user, seed_session, test_state};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn call(
        state: &Arc<AppState>,
        token: &str,
        method: &str,
        uri: &str,
    ) -> (StatusCode, String) {
        let app = router(state.clone()).with_state(state.clone());
        let res = app
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn member_cannot_see_or_delete_another_users_chat_plan() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "member").await;
        seed_session(&state, "s-mine", Some("u1"), None).await;
        seed_session(&state, "s-other", Some("u2"), None).await;
        let mine = state
            .db
            .upsert_plan("s-mine", None, None, "mine", "m")
            .await
            .unwrap();
        let other = state
            .db
            .upsert_plan("s-other", None, None, "other", "o")
            .await
            .unwrap();

        let (status, body) = call(&state, &token, "GET", "/api/plans").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains(&mine.id) && !body.contains(&other.id),
            "{body}"
        );

        let uri = format!("/api/plans/{}", other.id);
        assert_eq!(
            call(&state, &token, "GET", &uri).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(&state, &token, "DELETE", &uri).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(&state, &token, "GET", "/api/plans?session_id=s-other")
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        assert!(state.db.get_plan(&other.id).await.unwrap().is_some());

        let uri = format!("/api/plans/{}", mine.id);
        assert_eq!(call(&state, &token, "GET", &uri).await.0, StatusCode::OK);
    }
}
