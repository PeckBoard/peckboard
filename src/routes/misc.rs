use axum::{
    Json, Router,
    extract::State,
    middleware,
    response::IntoResponse,
    routing::get,
};
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::auth::middleware::require_auth;
use crate::state::AppState;

/// Global handle for the caffeinate process on macOS.
static CAFFEINATE: Mutex<Option<std::process::Child>> = Mutex::new(None);

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
                "id": "research",
                "name": "Research",
                "steps": ["backlog", "research", "summarize", "done"],
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
    let supported = cfg!(target_os = "macos");
    let active = CAFFEINATE
        .lock()
        .map(|guard| guard.is_some())
        .unwrap_or(false);

    Json(serde_json::json!({
        "supported": supported,
        "enabled": active,
        "active": active,
    }))
}

#[derive(Deserialize)]
struct KeepAwakeRequest {
    enabled: bool,
}

/// PUT /api/keep-awake — toggle keep-awake
async fn put_keep_awake(Json(req): Json<KeepAwakeRequest>) -> impl IntoResponse {
    let supported = cfg!(target_os = "macos");

    if !supported {
        return Json(serde_json::json!({
            "supported": false,
            "enabled": false,
            "active": false,
        }));
    }

    if req.enabled {
        // Spawn caffeinate if not already running
        let mut guard = CAFFEINATE.lock().unwrap();
        if guard.is_none() {
            let pid = std::process::id().to_string();
            match std::process::Command::new("caffeinate")
                .args(["-i", "-w", &pid])
                .spawn()
            {
                Ok(child) => {
                    tracing::info!(
                        caffeinate_pid = child.id(),
                        "Started caffeinate for keep-awake"
                    );
                    *guard = Some(child);
                    // Start watchdog to respawn caffeinate if it dies
                    tokio::spawn(async {
                        loop {
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            let mut guard = CAFFEINATE.lock().unwrap();
                            if let Some(ref mut child) = *guard {
                                match child.try_wait() {
                                    Ok(Some(_)) => {
                                        // Caffeinate died, respawn
                                        tracing::warn!("caffeinate process died, respawning");
                                        match std::process::Command::new("caffeinate")
                                            .args(["-i", "-w", &std::process::id().to_string()])
                                            .spawn()
                                        {
                                            Ok(new_child) => { *child = new_child; }
                                            Err(e) => {
                                                tracing::error!("Failed to respawn caffeinate: {e}");
                                                *guard = None;
                                                break;
                                            }
                                        }
                                    }
                                    Ok(None) => {} // Still running
                                    Err(_) => { *guard = None; break; }
                                }
                            } else {
                                break; // No process, stop watchdog
                            }
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to spawn caffeinate: {e}");
                    return Json(serde_json::json!({
                        "supported": true,
                        "enabled": false,
                        "active": false,
                        "error": format!("Failed to spawn caffeinate: {e}"),
                    }));
                }
            }
        }
    } else {
        // Kill caffeinate if running
        let mut guard = CAFFEINATE.lock().unwrap();
        if let Some(mut child) = guard.take() {
            tracing::info!(
                caffeinate_pid = child.id(),
                "Stopping caffeinate for keep-awake"
            );
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    let active = CAFFEINATE
        .lock()
        .map(|guard| guard.is_some())
        .unwrap_or(false);

    Json(serde_json::json!({
        "supported": true,
        "enabled": active,
        "active": active,
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
