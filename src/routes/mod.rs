pub mod agent_vars;
pub mod askpass;
pub mod attachments;
pub mod auth;
pub mod backup;
pub mod claude_accounts;
pub mod env_vars;
pub mod folders;
pub mod grok_accounts;
pub mod kimi_accounts;
pub mod mcp;
pub mod mcp_oauth;
pub mod me;
pub mod misc;
pub mod notifications;
pub mod ollama;
pub mod plans;
pub mod plugin_api;
pub mod plugin_ui;
pub mod plugins;
pub mod projects;
pub mod repeating_tasks;
pub mod reports;
pub mod sessions;
pub mod settings;
pub mod system_prompts;
pub mod update;
pub mod usage;

use crate::frontend::static_handler;
use crate::state::AppState;
use crate::ws::handler::ws_handler;
use axum::{Router, routing::get};
use std::sync::Arc;

pub fn api_router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/health", get(health))
        .route("/ws", get(ws_handler))
        // Sudo askpass bridge -- /api/askpass is token-authed (called by the
        // generated helper from inside sessions), the answer route is JWT'd.
        .merge(askpass::router(state.clone()))
        // MCP route -- no auth middleware, uses its own token auth + loopback gating
        .merge(mcp::router(state.clone()))
        // MCP-server OAuth: /api/* routes are JWT'd; GET /oauth/callback is
        // public — the provider redirect is claimed by its one-time state.
        .merge(mcp_oauth::router(state.clone()))
        .merge(auth::router(state.clone()))
        .merge(claude_accounts::router(state.clone()))
        .merge(grok_accounts::router(state.clone()))
        .merge(kimi_accounts::router(state.clone()))
        .merge(folders::router(state.clone()))
        .merge(sessions::router(state.clone()))
        .merge(projects::router(state.clone()))
        .merge(plans::router(state.clone()))
        .merge(repeating_tasks::router(state.clone()))
        .merge(reports::router(state.clone()))
        .merge(attachments::router(state.clone()))
        .merge(notifications::router(state.clone()))
        .merge(me::router(state.clone()))
        .merge(agent_vars::router(state.clone()))
        .merge(env_vars::router(state.clone()))
        .merge(settings::router(state.clone()))
        .merge(system_prompts::router(state.clone()))
        .merge(ollama::router(state.clone()))
        .merge(plugins::router(state.clone()))
        .merge(backup::router(state.clone()))
        // Public, plugin-owned HTTP surface. Intentionally NOT behind the
        // `/api/*` auth middleware — the serving plugin owns its own auth.
        .merge(plugin_api::router(state.clone()))
        // Authenticated plugin app-UI surface (behind `require_auth`); the
        // plugin acts under the logged-in user's authority.
        .merge(plugin_ui::router(state.clone()))
        .merge(usage::router(state.clone()))
        .merge(usage::trends::router(state.clone()))
        .merge(update::router(state.clone()))
        .merge(misc::router(state))
        .fallback(static_handler)
}

async fn health() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({ "ok": true }))
}
