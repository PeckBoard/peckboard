pub mod attachments;
pub mod auth;
pub mod folders;
pub mod git;
pub mod mcp;
pub mod me;
pub mod misc;
pub mod notifications;
pub mod projects;
pub mod repeating_tasks;
pub mod reports;
pub mod sessions;

use crate::frontend::static_handler;
use crate::state::AppState;
use crate::ws::handler::ws_handler;
use axum::{Router, routing::get};
use std::sync::Arc;

pub fn api_router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/health", get(health))
        .route("/ws", get(ws_handler))
        // MCP route -- no auth middleware, uses its own token auth + loopback gating
        .merge(mcp::router(state.clone()))
        .merge(auth::router(state.clone()))
        .merge(folders::router(state.clone()))
        .merge(sessions::router(state.clone()))
        .merge(projects::router(state.clone()))
        .merge(repeating_tasks::router(state.clone()))
        .merge(reports::router(state.clone()))
        .merge(git::router(state.clone()))
        .merge(attachments::router(state.clone()))
        .merge(notifications::router(state.clone()))
        .merge(me::router(state.clone()))
        .merge(misc::router(state))
        .fallback(static_handler)
}

async fn health() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({ "ok": true }))
}
