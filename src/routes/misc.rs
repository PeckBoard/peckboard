use axum::{Json, Router, extract::State, middleware, response::IntoResponse, routing::get};
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::auth::middleware::require_auth;
use crate::state::AppState;

/// State machine for the macOS `caffeinate` keep-awake feature.
///
/// The old design — `Mutex<Option<Child>>` plus a tokio::spawn'd
/// watchdog that polled every 5s — could leak watchdog tasks under
/// rapid enable/disable toggles: each enable spawned a new watchdog
/// without proving the old one had exited, so over N toggle cycles
/// you could end up with N sleeping watchdog tasks all racing to
/// detect process death and respawn. The state-enum + generation
/// counter makes the invariant explicit: at most one watchdog can
/// match the current generation, so any older watchdog wakes, sees
/// the generation has advanced, and exits.
struct CaffeinateState {
    child: Option<std::process::Child>,
    /// Monotonically increased every time the keep-awake feature is
    /// enabled. The watchdog captures the generation at spawn time
    /// and exits immediately if it ever observes a different value.
    generation: u64,
}

static CAFFEINATE: Mutex<CaffeinateState> = Mutex::new(CaffeinateState {
    child: None,
    generation: 0,
});

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/models", get(list_models))
        .route("/api/workflows", get(list_workflows))
        .route("/api/priorities", get(list_priorities))
        .route("/api/keep-awake", get(get_keep_awake).put(put_keep_awake))
        .route("/api/config", get(get_config).put(put_config))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// The canonical set of card priorities. Plugins can extend via the
/// `card.priorities.list` hook.
pub const DEFAULT_PRIORITIES: &[(&str, i32, &str)] = &[
    ("Critical", 0, "Blocks everything, fix immediately"),
    ("High", 1, "Important, do soon"),
    ("Medium", 2, "Normal priority"),
    ("Low", 3, "Nice to have"),
];

/// Validate that a priority value is in the allowed set.
pub fn is_valid_priority(value: i32) -> bool {
    DEFAULT_PRIORITIES.iter().any(|(_, v, _)| *v == value)
}

/// GET /api/models — list all available models across providers
async fn list_models(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // Resolve providers with their effective (settings-derived) model
    // lists once, then derive the flat list from the same snapshot so a
    // provider's `dynamic_models` is only computed a single time per call.
    let providers = state.provider_registry.list_providers_with_models().await;

    Json(serde_json::json!({
        "providers": providers.iter().map(|p| serde_json::json!({
            "id": p.id,
            "display_name": p.display_name,
            "models": p.models.iter().map(|m| serde_json::json!({
                "id": format!("{}:{}", p.id, m.id),
                "display_name": m.display_name,
                "capabilities": m.capabilities,
            })).collect::<Vec<_>>(),
            "effort_levels": p.effort_levels.iter().map(|e| serde_json::json!({
                "id": e.id,
                "label": e.label,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "models": providers.iter().flat_map(|p| p.models.iter().map(move |m| serde_json::json!({
            "id": format!("{}:{}", p.id, m.id),
            "display_name": m.display_name,
            "capabilities": m.capabilities,
        }))).collect::<Vec<_>>(),
    }))
}

/// GET /api/workflows — list built-in workflow definitions
async fn list_workflows() -> impl IntoResponse {
    Json(serde_json::json!({ "workflows": crate::workflow::WORKFLOWS }))
}

/// GET /api/priorities — list card priority levels
async fn list_priorities(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut priorities: Vec<serde_json::Value> = DEFAULT_PRIORITIES
        .iter()
        .map(|(label, value, description)| {
            serde_json::json!({
                "label": label,
                "value": value,
                "description": description,
            })
        })
        .collect();

    // Hook: card.priorities.list — plugins can add/modify priorities
    let hook_result = state
        .plugins
        .dispatch(
            "card.priorities.list",
            serde_json::json!({ "priorities": priorities }),
        )
        .await;
    if let crate::plugin::hooks::HookResult::Allowed(modified) = hook_result {
        if let Some(p) = modified.get("priorities").and_then(|v| v.as_array()) {
            priorities = p.clone();
        }
    }

    Json(serde_json::json!({ "priorities": priorities }))
}

/// GET /api/keep-awake — returns keep-awake status
async fn get_keep_awake() -> impl IntoResponse {
    let supported = cfg!(target_os = "macos");
    let active = CAFFEINATE
        .lock()
        .map(|guard| guard.child.is_some())
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
        // Spawn caffeinate if not already running, and start a watchdog
        // tagged with a fresh generation. Disable bumps the generation
        // so any leftover watchdog (e.g. sleeping mid-poll during a
        // rapid toggle) exits immediately when it wakes.
        let mut guard = CAFFEINATE.lock().unwrap();
        if guard.child.is_none() {
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
                    guard.child = Some(child);
                    guard.generation = guard.generation.wrapping_add(1);
                    let my_generation = guard.generation;
                    drop(guard);

                    tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            let mut guard = CAFFEINATE.lock().unwrap();
                            // A different generation means this
                            // watchdog is stale; another enable cycle
                            // has its own watchdog now.
                            if guard.generation != my_generation {
                                break;
                            }
                            let Some(ref mut child) = guard.child else {
                                break;
                            };
                            match child.try_wait() {
                                Ok(Some(_)) => {
                                    tracing::warn!("caffeinate process died, respawning");
                                    match std::process::Command::new("caffeinate")
                                        .args(["-i", "-w", &std::process::id().to_string()])
                                        .spawn()
                                    {
                                        Ok(new_child) => *child = new_child,
                                        Err(e) => {
                                            tracing::error!("Failed to respawn caffeinate: {e}");
                                            guard.child = None;
                                            break;
                                        }
                                    }
                                }
                                Ok(None) => {} // still running
                                Err(_) => {
                                    guard.child = None;
                                    break;
                                }
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
        // Kill caffeinate if running and bump the generation so any
        // sleeping watchdog will exit on next wake.
        let mut guard = CAFFEINATE.lock().unwrap();
        if let Some(mut child) = guard.child.take() {
            tracing::info!(
                caffeinate_pid = child.id(),
                "Stopping caffeinate for keep-awake"
            );
            let _ = child.kill();
            let _ = child.wait();
        }
        guard.generation = guard.generation.wrapping_add(1);
    }

    let active = CAFFEINATE
        .lock()
        .map(|guard| guard.child.is_some())
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
