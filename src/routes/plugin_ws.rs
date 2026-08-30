//! `POST /api/plugin-ws/ticket` — mint a one-time ticket for the restricted
//! plugin-page WebSocket (`/ws/plugin-ui`, see [`crate::ws::plugin_ui`]).
//!
//! Only the logged-in app calls this (behind `require_auth`), and it does so
//! on a plugin page's behalf with the page's OWN plugin id (the parent frame
//! fills it from its `plugin` prop — `web/src/components/PluginFullPage.tsx`),
//! so a sandboxed page can never obtain a ticket scoped to another plugin.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::post,
};

use crate::auth::middleware::{AuthUser, require_auth};
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/plugin-ws/ticket", post(issue_ticket))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

#[derive(serde::Deserialize)]
struct TicketRequest {
    plugin_id: String,
}

async fn issue_ticket(
    State(state): State<Arc<AppState>>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Json(req): Json<TicketRequest>,
) -> Response {
    let plugin_id = req.plugin_id.trim().to_string();
    // Only an approved, loaded plugin gets a stream: an inert (pending /
    // denied) plugin's pages don't serve, so nothing legitimate connects.
    let known = state
        .plugins
        .wasm_plugins()
        .await
        .iter()
        .any(|p| p.name == plugin_id && p.status == "approved");
    if !known {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no such plugin" })),
        )
            .into_response();
    }
    match state.plugin_ws_tickets.issue(&plugin_id, &user.session_id) {
        Some(ticket) => Json(serde_json::json!({ "ticket": ticket })).into_response(),
        None => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({ "error": "too many outstanding tickets" })),
        )
            .into_response(),
    }
}
