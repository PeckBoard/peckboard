use axum::{
    Json, Router,
    extract::State,
    middleware,
    response::IntoResponse,
    routing::get,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::auth::middleware::require_auth;
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/models", get(list_models))
        .route("/api/workflows", get(list_workflows))
        .route("/api/keep-awake", get(get_keep_awake).put(put_keep_awake))
        .route("/api/config", get(get_config).put(put_config))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// GET /api/models — list all available models across providers
async fn list_models(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let providers = state.provider_registry.list_providers().await;
    let all_models = state.provider_registry.list_all_models().await;

    Json(serde_json::json!({
        "providers": providers.iter().map(|p| serde_json::json!({
            "id": p.id,
            "display_name": p.display_name,
            "models": p.models.iter().map(|m| serde_json::json!({
                "id": format!("{}:{}", p.id, m.id),
                "display_name": m.display_name,
                "capabilities": m.capabilities,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "models": all_models.iter().map(|(full_id, m)| serde_json::json!({
            "id": full_id,
            "display_name": m.display_name,
            "capabilities": m.capabilities,
        })).collect::<Vec<_>>(),
    }))
}

/// GET /api/workflows — list built-in workflow definitions
async fn list_workflows() -> impl IntoResponse {
    Json(serde_json::json!({
        "workflows": [
            {
                "id": "default",
                "name": "Default",
                "steps": ["backlog", "in_progress", "review", "done"],
            },
            {
                "id": "simple",
                "name": "Simple",
                "steps": ["backlog", "in_progress", "done"],
            },
            {
                "id": "full",
                "name": "Full Pipeline",
                "steps": ["backlog", "design", "implement", "test", "review", "done"],
            },
        ]
    }))
}

/// GET /api/keep-awake — returns keep-awake status
async fn get_keep_awake() -> impl IntoResponse {
    // Keep-awake is a platform-dependent feature; report as unsupported stub for now
    Json(serde_json::json!({
        "supported": false,
        "enabled": false,
        "active": false,
    }))
}

#[derive(Deserialize)]
struct KeepAwakeRequest {
    #[allow(dead_code)]
    enabled: bool,
}

/// PUT /api/keep-awake — toggle keep-awake
async fn put_keep_awake(Json(req): Json<KeepAwakeRequest>) -> impl IntoResponse {
    // Stub: acknowledge the request but keep-awake is not yet implemented
    Json(serde_json::json!({
        "supported": false,
        "enabled": req.enabled,
        "active": false,
    }))
}

/// GET /api/config — returns current config values
async fn get_config(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let config = &state.config;
    Json(serde_json::json!({
        "port": config.port,
        "https_port": config.https_port,
        "host": config.host,
        "data_dir": config.data_dir.to_string_lossy(),
    }))
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct UpdateConfigRequest {
    port: Option<u16>,
    https_port: Option<u16>,
    host: Option<String>,
}

/// PUT /api/config — update config values
async fn put_config(
    State(state): State<Arc<AppState>>,
    Json(_req): Json<UpdateConfigRequest>,
) -> impl IntoResponse {
    // Config is currently immutable at runtime; return current values.
    // A future implementation could persist changes and trigger a restart.
    let config = &state.config;
    Json(serde_json::json!({
        "port": config.port,
        "https_port": config.https_port,
        "host": config.host,
        "data_dir": config.data_dir.to_string_lossy(),
        "message": "Config updates require a restart to take effect (not yet implemented).",
    }))
}
