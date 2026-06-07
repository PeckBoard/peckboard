//! `/api/me/*` — per-user data not tied to a specific resource.
//!
//! Currently only the cross-device tab list. The frontend tab strip
//! fetches `GET /api/me/tabs` on mount and on window focus, POSTs to
//! upsert when a tab is opened (this both creates a new tab and bumps
//! `last_active` so MRU ordering + read state stays in sync across
//! devices), and DELETEs to close.

use axum::body::Body;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{Request, StatusCode},
    middleware,
    response::IntoResponse,
    routing::{delete, get},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::auth::middleware::{AuthUser, require_auth};
use crate::state::AppState;

#[derive(Serialize)]
pub struct TabView {
    pub item_type: String,
    pub item_id: String,
    pub last_active: String,
}

#[derive(Deserialize)]
pub struct UpsertTabRequest {
    pub item_type: String,
    pub item_id: String,
}

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/me/tabs", get(list_tabs).post(upsert_tab))
        .route("/api/me/tabs/{item_type}/{item_id}", delete(delete_tab))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

fn auth_user(req: &Request<Body>) -> &AuthUser {
    req.extensions()
        .get::<AuthUser>()
        .expect("auth middleware should inject AuthUser")
}

fn validate_item_type(s: &str) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    match s {
        "session" | "project" => Ok(()),
        _ => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "item_type must be 'session' or 'project'" })),
        )),
    }
}

/// GET /api/me/tabs — list this user's tabs, MRU first.
async fn list_tabs(State(state): State<Arc<AppState>>, req: Request<Body>) -> impl IntoResponse {
    let user = auth_user(&req);
    match state.db.list_user_tabs(&user.user_id).await {
        Ok(tabs) => Ok(Json(
            tabs.into_iter()
                .map(|t| TabView {
                    item_type: t.item_type,
                    item_id: t.item_id,
                    last_active: t.last_active,
                })
                .collect::<Vec<_>>(),
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}

/// POST /api/me/tabs — open or activate a tab.
///
/// Idempotent: same body twice in a row just bumps `last_active`. This
/// is what the frontend calls when the user clicks any tab (including
/// one already in the strip), which both reorders MRU and counts as
/// "viewed" for the unread badge.
async fn upsert_tab(State(state): State<Arc<AppState>>, req: Request<Body>) -> impl IntoResponse {
    let user_id = auth_user(&req).user_id.clone();
    // Have to take the body separately because we already borrowed `req`.
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, 1024).await {
        Ok(b) => b,
        Err(_) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "body too large" })),
            ));
        }
    };
    let _ = parts; // unused
    let req_body: UpsertTabRequest = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid JSON: {e}") })),
            ));
        }
    };
    validate_item_type(&req_body.item_type)?;

    match state
        .db
        .upsert_user_tab(&user_id, &req_body.item_type, &req_body.item_id)
        .await
    {
        Ok(tab) => Ok(Json(TabView {
            item_type: tab.item_type,
            item_id: tab.item_id,
            last_active: tab.last_active,
        })),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}

/// DELETE /api/me/tabs/:item_type/:item_id — close a tab (without
/// touching the underlying session/project).
async fn delete_tab(
    State(state): State<Arc<AppState>>,
    Path((item_type, item_id)): Path<(String, String)>,
    req: Request<Body>,
) -> impl IntoResponse {
    let user_id = auth_user(&req).user_id.clone();
    validate_item_type(&item_type)?;

    match state
        .db
        .delete_user_tab(&user_id, &item_type, &item_id)
        .await
    {
        Ok(_) => Ok(StatusCode::NO_CONTENT),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}
