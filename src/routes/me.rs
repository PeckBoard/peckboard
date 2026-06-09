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
    /// Name of the referenced session/project, denormalized so the tab
    /// strip doesn't have to cross-reference a separate list. This also
    /// lets us include worker sessions (`is_worker=true`) — those are
    /// not surfaced by `GET /api/sessions`, so before we shipped this
    /// the tab strip rendered them as a phantom "Session" chip and the
    /// auto-cleanup loop closed them as soon as the sessions list
    /// loaded.
    pub name: String,
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
///
/// Each tab is enriched with the underlying session/project name so the
/// strip can render a label without cross-referencing a separate list.
/// Tabs whose referenced item no longer exists are filtered out — this
/// is the cross-device cleanup path for sessions/projects deleted on
/// another device.
///
/// Implementation note: the per-tab name lookup is N+1 against
/// `sessions` / `projects`. Acceptable for the expected tab count
/// (humans rarely keep more than a couple dozen open) and avoids a
/// hand-rolled polymorphic JOIN. If users start running into 100+
/// tabs we can swap this for a single batched lookup keyed by id.
async fn list_tabs(State(state): State<Arc<AppState>>, req: Request<Body>) -> impl IntoResponse {
    let user = auth_user(&req);
    let tabs = match state.db.list_user_tabs(&user.user_id).await {
        Ok(t) => t,
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            ));
        }
    };

    let mut out: Vec<TabView> = Vec::with_capacity(tabs.len());
    for t in tabs {
        let name = match t.item_type.as_str() {
            "session" => state
                .db
                .get_session(&t.item_id)
                .await
                .ok()
                .flatten()
                .map(|s| s.name),
            "project" => state
                .db
                .get_project(&t.item_id)
                .await
                .ok()
                .flatten()
                .map(|p| p.name),
            _ => None,
        };
        if let Some(name) = name {
            out.push(TabView {
                item_type: t.item_type,
                item_id: t.item_id,
                last_active: t.last_active,
                name,
            });
        }
    }
    Ok(Json(out))
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
        Ok(Some(tab)) => {
            // Look up the underlying name so the response shape matches
            // GET /api/me/tabs (same denormalized `name` field). The
            // upsert path has already verified the item exists, so a
            // missing name here would be a TOCTOU race — fall back to
            // empty rather than 500ing.
            let name = match tab.item_type.as_str() {
                "session" => state
                    .db
                    .get_session(&tab.item_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|s| s.name)
                    .unwrap_or_default(),
                "project" => state
                    .db
                    .get_project(&tab.item_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|p| p.name)
                    .unwrap_or_default(),
                _ => String::new(),
            };
            Ok(Json(TabView {
                item_type: tab.item_type,
                item_id: tab.item_id,
                last_active: tab.last_active,
                name,
            }))
        }
        // Item doesn't exist — refuse to create the tab. Stops phantom
        // chips from being written for stale URLs or cross-device delete
        // races. The frontend treats 404 here as "drop the local tab".
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "referenced item does not exist" })),
        )),
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
