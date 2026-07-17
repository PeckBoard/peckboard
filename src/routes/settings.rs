//! `/api/settings/*` — app-level settings surfaced to the authenticated user.
//!
//! - Approved commands: the programs granted a persistent "always" approval
//!   by `run_command` (stored under the `core.common-tools` plugin id,
//!   `cli_always` collection). The UI reads them so grants can be reviewed
//!   and revoked.
//! - Caveman mode: global output-style level (`off` | `lite` | `full`) for
//!   interactive sessions, stored in the same plugin-store KV under
//!   [`SETTINGS_NS`]/[`SETTINGS_COLLECTION`] and read at dispatch time by
//!   `SessionManager::send_message_locked`.
//! - Pre-hatcher model: which model the pre-hatcher plugin researches on
//!   (`{"model": ...}`; empty = auto, the provider's cheapest priced model),
//!   read per turn by the `session.message.before` dispatch path.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{delete, get, post, put},
};
use std::collections::HashSet;
use std::sync::Arc;

use crate::auth::middleware::require_auth;
use crate::db::Db;
use crate::service::mcp_server::user_servers;
use crate::state::AppState;

/// Plugin id / collection the native `run_command` tool records "always"
/// approvals under (see `service::mcp_server::common_tools`).
const NS: &str = "core.common-tools";
const ALWAYS_COLLECTION: &str = "cli_always";

/// Plugin-store namespace for core app settings (shared with the dispatch
/// path in `provider::manager`, which reads `caveman_mode` per turn).
pub const SETTINGS_NS: &str = "core.settings";
pub const SETTINGS_COLLECTION: &str = "app";

const CAVEMAN_LEVELS: &[&str] = &["off", "lite", "full"];

/// Plugin-store key for the pre-hatcher research-model override
/// (`{"model": "provider:model"}`; empty/missing ⇒ auto — the provider's
/// cheapest priced model).
const PRE_HATCHER_MODEL_KEY: &str = "pre_hatcher_model";

/// Plugin-store key for the pre-hatcher research system-prompt selection — a
/// library prompt NAME (see `system_prompts`). Empty/missing ⇒ the default.
const PRE_HATCHER_SYSTEM_PROMPT_KEY: &str = "pre_hatcher_system_prompt";

/// The pre-hatcher's default research system prompt when none is configured.
/// Resolved to its body at dispatch time; falls back to no override if the
/// named prompt has been deleted from the library.
pub const PRE_HATCHER_DEFAULT_SYSTEM_PROMPT: &str = "fable 5";
const HIDDEN_PROVIDERS_KEY: &str = "hidden_providers";

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/settings/approved-commands", get(list_approved))
        .route(
            "/api/settings/approved-commands/{program}",
            delete(delete_approved),
        )
        .route("/api/settings/caveman", get(get_caveman).put(set_caveman))
        .route(
            "/api/settings/pre-hatcher",
            get(get_pre_hatcher).put(set_pre_hatcher),
        )
        .route(
            "/api/settings/pre-hatcher-prompt",
            get(get_pre_hatcher_prompt).put(set_pre_hatcher_prompt),
        )
        .route("/api/settings/providers", get(get_providers))
        .route("/api/settings/providers/{id}", put(set_provider_hidden))
        .route(
            "/api/settings/mcp-servers",
            get(get_mcp_servers).put(set_mcp_servers),
        )
        .route(
            "/api/settings/mcp-servers/check-command",
            post(check_mcp_command),
        )
        .route("/api/settings/mcp-servers/probe", post(probe_mcp_server))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// GET /api/settings/caveman → `{"level":"off|lite|full"}` (default "off").
async fn get_caveman(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let db = state.db.clone();
    let raw = tokio::task::spawn_blocking(move || {
        db.plugin_store_get_blocking(SETTINGS_NS, SETTINGS_COLLECTION, "caveman_mode")
    })
    .await;
    let level = match raw {
        Ok(Ok(Some(json))) => serde_json::from_str::<serde_json::Value>(&json)
            .ok()
            .and_then(|v| v.get("level").and_then(|l| l.as_str()).map(str::to_string))
            .unwrap_or_else(|| "off".into()),
        _ => "off".into(),
    };
    Json(serde_json::json!({ "level": level }))
}

#[derive(serde::Deserialize)]
struct CavemanBody {
    level: String,
}

/// PUT /api/settings/caveman `{"level":"off|lite|full"}` → 204. Takes effect
/// on each session's next dispatched turn.
async fn set_caveman(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CavemanBody>,
) -> impl IntoResponse {
    if !CAVEMAN_LEVELS.contains(&body.level.as_str()) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "level must be off, lite, or full" })),
        ));
    }
    let db = state.db.clone();
    let value = serde_json::json!({ "level": body.level }).to_string();
    let res = tokio::task::spawn_blocking(move || {
        db.plugin_store_put_blocking(SETTINGS_NS, SETTINGS_COLLECTION, "caveman_mode", &value)
    })
    .await;
    match res {
        Ok(Ok(_)) => Ok(StatusCode::NO_CONTENT),
        Ok(Err(e)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}

/// The pre-hatcher research-model override, or `None` when unset/empty
/// (auto — dispatch falls back to the provider's cheapest priced model).
/// Read per turn by the `session.message.before` dispatch path.
pub async fn pre_hatcher_model(state: &Arc<AppState>) -> Option<String> {
    let db = state.db.clone();
    let raw = tokio::task::spawn_blocking(move || {
        db.plugin_store_get_blocking(SETTINGS_NS, SETTINGS_COLLECTION, PRE_HATCHER_MODEL_KEY)
    })
    .await;
    match raw {
        Ok(Ok(Some(json))) => serde_json::from_str::<serde_json::Value>(&json)
            .ok()
            .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(str::to_string))
            .filter(|m| !m.trim().is_empty()),
        _ => None,
    }
}

/// GET /api/settings/pre-hatcher → `{"model": "provider:model" | ""}` ("" =
/// auto: the session provider's cheapest priced model).
async fn get_pre_hatcher(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let model = pre_hatcher_model(&state).await.unwrap_or_default();
    Json(serde_json::json!({ "model": model }))
}

#[derive(serde::Deserialize)]
struct PreHatcherBody {
    model: String,
}

/// PUT /api/settings/pre-hatcher `{"model": "provider:model" | ""}` → 204.
/// Empty clears the override (auto). Takes effect on each chat's next
/// message.
async fn set_pre_hatcher(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PreHatcherBody>,
) -> impl IntoResponse {
    let model = body.model.trim().to_string();
    if model.chars().count() > 200 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "model id too long" })),
        ));
    }
    let db = state.db.clone();
    let value = serde_json::json!({ "model": model }).to_string();
    let res = tokio::task::spawn_blocking(move || {
        db.plugin_store_put_blocking(
            SETTINGS_NS,
            SETTINGS_COLLECTION,
            PRE_HATCHER_MODEL_KEY,
            &value,
        )
    })
    .await;
    match res {
        Ok(Ok(_)) => Ok(StatusCode::NO_CONTENT),
        Ok(Err(e)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}

/// The pre-hatcher research system-prompt NAME: the configured library name,
/// or [`PRE_HATCHER_DEFAULT_SYSTEM_PROMPT`] when unset/empty. Read per turn by
/// the `session.message.before` dispatch path, which resolves it to a body.
pub async fn pre_hatcher_system_prompt_name(state: &Arc<AppState>) -> String {
    let db = state.db.clone();
    let raw = tokio::task::spawn_blocking(move || {
        db.plugin_store_get_blocking(
            SETTINGS_NS,
            SETTINGS_COLLECTION,
            PRE_HATCHER_SYSTEM_PROMPT_KEY,
        )
    })
    .await;
    let configured = match raw {
        Ok(Ok(Some(json))) => serde_json::from_str::<serde_json::Value>(&json)
            .ok()
            .and_then(|v| v.get("name").and_then(|m| m.as_str()).map(str::to_string))
            .filter(|m| !m.trim().is_empty()),
        _ => None,
    };
    configured.unwrap_or_else(|| PRE_HATCHER_DEFAULT_SYSTEM_PROMPT.to_string())
}

/// GET /api/settings/pre-hatcher-prompt → `{"name": "fable 5"}` (the effective
/// name, defaulting when unset).
async fn get_pre_hatcher_prompt(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let name = pre_hatcher_system_prompt_name(&state).await;
    Json(serde_json::json!({ "name": name }))
}

#[derive(serde::Deserialize)]
struct PreHatcherPromptBody {
    name: String,
}

/// PUT /api/settings/pre-hatcher-prompt `{"name": "fable 5"}` → 204. Empty
/// clears the override (reverts to the default). Takes effect on each chat's
/// next message. The name is validated at dispatch time — an unknown name
/// simply resolves to no system-prompt override.
async fn set_pre_hatcher_prompt(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PreHatcherPromptBody>,
) -> impl IntoResponse {
    let name = body.name.trim().to_string();
    if name.chars().count() > 200 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "name too long" })),
        ));
    }
    let db = state.db.clone();
    let value = serde_json::json!({ "name": name }).to_string();
    let res = tokio::task::spawn_blocking(move || {
        db.plugin_store_put_blocking(
            SETTINGS_NS,
            SETTINGS_COLLECTION,
            PRE_HATCHER_SYSTEM_PROMPT_KEY,
            &value,
        )
    })
    .await;
    match res {
        Ok(Ok(_)) => Ok(StatusCode::NO_CONTENT),
        Ok(Err(e)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}

/// GET /api/settings/approved-commands → `{"programs":[...]}`, sorted asc.
async fn list_approved(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let db = state.db.clone();
    let rows =
        tokio::task::spawn_blocking(move || db.plugin_store_list_blocking(NS, ALWAYS_COLLECTION))
            .await;

    match rows {
        Ok(Ok(rows)) => {
            // The list is already key-ascending from the DB, but sort again
            // defensively so the contract holds regardless of storage order.
            let mut programs: Vec<String> = rows.into_iter().map(|(key, _)| key).collect();
            programs.sort();
            Ok(Json(serde_json::json!({ "programs": programs })))
        }
        Ok(Err(e)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}

/// DELETE /api/settings/approved-commands/{program} → 204. Revokes an
/// "always" grant; a missing program is a no-op (still 204).
async fn delete_approved(
    State(state): State<Arc<AppState>>,
    Path(program): Path<String>,
) -> impl IntoResponse {
    let db = state.db.clone();
    let res = tokio::task::spawn_blocking(move || {
        db.plugin_store_delete_blocking(NS, ALWAYS_COLLECTION, &program)
    })
    .await;

    match res {
        Ok(Ok(_)) => Ok(StatusCode::NO_CONTENT),
        Ok(Err(e)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}

/// Read the hidden provider ids from the plugin store.
/// Returns an empty set on missing/parse error (nothing hidden by default).
pub(crate) async fn hidden_providers_for_db(db: Db) -> HashSet<String> {
    let raw = tokio::task::spawn_blocking(move || {
        db.plugin_store_get_blocking(SETTINGS_NS, SETTINGS_COLLECTION, HIDDEN_PROVIDERS_KEY)
    })
    .await;
    match raw {
        Ok(Ok(Some(json))) => serde_json::from_str::<serde_json::Value>(&json)
            .ok()
            .and_then(|v| {
                v.get("ids").and_then(|ids| ids.as_array()).map(|arr| {
                    arr.iter()
                        .filter_map(|id| id.as_str().map(str::to_string))
                        .collect()
                })
            })
            .unwrap_or_default(),
        _ => HashSet::new(),
    }
}

/// Returns the set of hidden provider ids. Empty → nothing hidden (default).
pub async fn hidden_providers(state: &Arc<AppState>) -> HashSet<String> {
    hidden_providers_for_db(state.db.clone()).await
}

/// GET /api/settings/providers → `{"providers":[{"id","display_name","hidden"}]}`
/// All registered providers (static list), sorted by display_name.
async fn get_providers(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let hidden = hidden_providers(&state).await;
    let mut providers = state.provider_registry.list_providers().await;
    providers.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    Json(serde_json::json!({
        "providers": providers.iter().map(|p| serde_json::json!({
            "id": p.id,
            "display_name": p.display_name,
            "hidden": hidden.contains(&p.id),
        })).collect::<Vec<_>>(),
    }))
}

#[derive(serde::Deserialize)]
struct ProviderHiddenBody {
    hidden: bool,
}

/// PUT /api/settings/providers/{id} `{"hidden": bool}` → 204.
/// 404 if the provider id is not in the registry.
async fn set_provider_hidden(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<ProviderHiddenBody>,
) -> impl IntoResponse {
    if state.provider_registry.get_info(&id).await.is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "unknown provider" })),
        ));
    }
    let db = state.db.clone();
    let hidden = body.hidden;
    let res = tokio::task::spawn_blocking(move || {
        let current_json =
            db.plugin_store_get_blocking(SETTINGS_NS, SETTINGS_COLLECTION, HIDDEN_PROVIDERS_KEY)?;
        let mut ids: HashSet<String> = current_json
            .as_deref()
            .and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok())
            .and_then(|v| {
                v.get("ids").and_then(|arr| arr.as_array()).map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
            })
            .unwrap_or_default();
        if hidden {
            ids.insert(id);
        } else {
            ids.remove(&id);
        }
        let mut sorted: Vec<String> = ids.into_iter().collect();
        sorted.sort();
        let value = serde_json::json!({ "ids": sorted }).to_string();
        db.plugin_store_put_blocking(
            SETTINGS_NS,
            SETTINGS_COLLECTION,
            HIDDEN_PROVIDERS_KEY,
            &value,
        )
    })
    .await;
    match res {
        Ok(Ok(_)) => Ok(StatusCode::NO_CONTENT),
        Ok(Err(e)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}

/// GET /api/settings/mcp-servers → the user-defined MCP server list plus
/// which providers can consume it (the UI greys out the rest).
async fn get_mcp_servers(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let servers = user_servers::load(&state.db).await;
    Json(serde_json::json!({
        "servers": servers,
        "supported_providers": user_servers::MCP_SUPPORTED_PROVIDERS,
    }))
}

#[derive(serde::Deserialize)]
struct McpServersBody {
    servers: Vec<user_servers::UserMcpServer>,
}

/// PUT /api/settings/mcp-servers `{"servers":[...]}` → 204. Validated as a
/// whole list; applies from each session's next dispatched turn (the
/// per-session config file is rewritten before every turn, see
/// `service::mcp_server::user_servers`).
async fn set_mcp_servers(
    State(state): State<Arc<AppState>>,
    Json(body): Json<McpServersBody>,
) -> impl IntoResponse {
    if let Err(msg) = user_servers::validate(&body.servers) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": msg })),
        ));
    }
    let value = match serde_json::to_string(&body.servers) {
        Ok(v) => v,
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            ));
        }
    };
    let db = state.db.clone();
    let res = tokio::task::spawn_blocking(move || {
        db.plugin_store_put_blocking(
            SETTINGS_NS,
            SETTINGS_COLLECTION,
            user_servers::MCP_SERVERS_KEY,
            &value,
        )
    })
    .await;
    match res {
        Ok(Ok(_)) => Ok(StatusCode::NO_CONTENT),
        Ok(Err(e)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}

/// POST /api/settings/mcp-servers/probe — connect to ONE server entry (saved
/// or a yet-unsaved editor draft) and list its tools. Always 200: a dead
/// server is a result (`{"ok":false,"error"}`), not a transport error. The
/// stdio probe runs the configured command server-side — the same trust model
/// as dispatch, which already launches every enabled server each turn.
async fn probe_mcp_server(
    State(state): State<Arc<AppState>>,
    Json(server): Json<user_servers::UserMcpServer>,
) -> impl IntoResponse {
    if let Err(msg) = user_servers::validate(std::slice::from_ref(&server)) {
        return Json(serde_json::json!({ "ok": false, "error": msg }));
    }
    let mut entry = user_servers::entry_json(&server);
    // OAuth servers probe with the same injected Authorization header a
    // session would get — including the just-connected, not-yet-saved case.
    if server.auth == "oauth" {
        match crate::service::mcp_server::oauth::bearer_for_server(&state.db, &server).await {
            Some(bearer) => {
                entry["headers"]["Authorization"] = serde_json::Value::String(bearer);
            }
            None => {
                return Json(serde_json::json!({
                    "ok": false,
                    "error": "not signed in yet — use the Sign in button above first",
                }));
            }
        }
    }
    let probe = async {
        let mut client = crate::service::mcp_client::McpClient::connect(&server.name, &entry)
            .await
            .map_err(|e| e.to_string())?;
        client
            .list_tools()
            .await
            .map_err(|e| format!("connected, but tools/list failed: {e}"))
    };
    // The client's own SETUP_TIMEOUT covers each request; this caps the whole
    // probe so the settings UI never hangs on a slow-to-die process.
    let result = tokio::time::timeout(std::time::Duration::from_secs(20), probe).await;
    let payload = match result {
        Ok(Ok(tools)) => serde_json::json!({
            "ok": true,
            "tools": tools
                .iter()
                .map(|t| serde_json::json!({ "name": t.name, "description": t.description }))
                .collect::<Vec<_>>(),
        }),
        Ok(Err(e)) => serde_json::json!({ "ok": false, "error": e }),
        Err(_) => serde_json::json!({ "ok": false, "error": "probe timed out after 20 seconds" }),
    };
    Json(payload)
}

#[derive(serde::Deserialize)]
struct CheckCommandBody {
    command: String,
}

/// POST /api/settings/mcp-servers/check-command — does a stdio server's
/// `command` exist on this host's PATH? Returns install hints and a
/// suggested working folder for a one-off install session when it doesn't.
async fn check_mcp_command(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CheckCommandBody>,
) -> impl IntoResponse {
    use crate::service::mcp_server::command_check;
    let checked = command_check::check_command(&body.command);
    let suggested = command_check::suggested_install_folder(&body.command, &state.config.data_dir);
    Json(serde_json::json!({
        "found": checked.found,
        "resolved_path": checked.resolved_path.map(|p| p.to_string_lossy().to_string()),
        "hints": command_check::install_hints(&body.command),
        "suggested_folder_path": suggested.to_string_lossy().to_string(),
    }))
}
