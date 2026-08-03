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
//!
//! The surface is split in two: [`admin_router`] holds everything that reads
//! or mutates host-wide state (the Claude permission gate, the approved-command
//! list, the MCP server list and its program-spawning probe routes) and is
//! admin-only; [`user_router`] holds the rest, open to any authenticated user.

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

use crate::auth::middleware::{require_admin, require_auth};
use crate::db::Db;
use crate::service::mcp_server::user_servers;
use crate::service::tls::{self, TlsSource};
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

/// Plugin-store key for the app-wide default model (`{"model":
/// "provider:model"}`; empty/missing ⇒ effort-based routing). Sessions,
/// cards, and reviews dispatched without an explicit model resolve to this
/// in `provider::manager::send_message_locked`.
pub const DEFAULT_MODEL_KEY: &str = "default_model";

/// Plugin-store key for the pre-hatcher research system-prompt selection — a
/// library prompt NAME (see `system_prompts`). Empty/missing ⇒ the default.
const PRE_HATCHER_SYSTEM_PROMPT_KEY: &str = "pre_hatcher_system_prompt";

/// The pre-hatcher's default research system prompt when none is configured.
/// Resolved to its body at dispatch time; falls back to no override if the
/// named prompt has been deleted from the library.
pub const PRE_HATCHER_DEFAULT_SYSTEM_PROMPT: &str = "fable 5";
const HIDDEN_PROVIDERS_KEY: &str = "hidden_providers";

/// Plugin-store key for data-retention limits (repeating-task run sessions,
/// terminal-session events, report files). See [`RetentionSettings`].
pub const RETENTION_KEY: &str = "retention";

/// Plugin-store key for the Claude permission-bypass escape hatch
/// (`{"bypass": bool}`; missing ⇒ false = enforced). See
/// `claude_bypass_permissions_for_db`.
const CLAUDE_BYPASS_KEY: &str = "claude_bypass_permissions";

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    admin_router()
        .merge(user_router())
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// Settings that read or mutate **host-wide** state: the Claude permission
/// gate (`--dangerously-skip-permissions` for every project and every user on
/// this host), the persistent `run_command` approval list, the global MCP
/// server list — whose probe/check-command routes spawn a program named in the
/// request body — and the TLS routes, which read and replace the key material
/// the whole host is served with.
/// None of that is partitioned per user, so an admin-created non-admin must
/// not be able to reach it.
///
/// Layers run outer-to-inner on the request, so `require_admin` is appended
/// here and `require_auth` in [`router`] afterwards, which puts `AuthUser`
/// into the extensions before this middleware reads it.
fn admin_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/settings/approved-commands", get(list_approved))
        .route(
            "/api/settings/approved-commands/{program}",
            delete(delete_approved),
        )
        .route(
            "/api/settings/claude-permissions",
            get(get_claude_permissions).put(set_claude_permissions),
        )
        .route(
            "/api/settings/mcp-servers",
            get(get_mcp_servers).put(set_mcp_servers),
        )
        .route(
            "/api/settings/mcp-servers/check-command",
            post(check_mcp_command),
        )
        .route("/api/settings/mcp-servers/probe", post(probe_mcp_server))
        .route(
            "/api/settings/retention",
            get(get_retention).put(set_retention),
        )
        .route("/api/settings/tls", get(get_tls))
        .route(
            "/api/settings/tls/cert",
            post(upload_tls_cert).delete(delete_tls_cert),
        )
        .route("/api/settings/tls/regenerate", post(regenerate_tls_cert))
        .route_layer(middleware::from_fn(require_admin))
}

/// Settings any authenticated user may read and change. These are still
/// host-wide values rather than per-user preferences (see
/// `docs/auth-security.md`), but they only steer agent output style and model
/// pickers — they can't loosen a security boundary or execute anything.
fn user_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/settings/caveman", get(get_caveman).put(set_caveman))
        .route(
            "/api/settings/pre-hatcher",
            get(get_pre_hatcher).put(set_pre_hatcher),
        )
        .route(
            "/api/settings/pre-hatcher-prompt",
            get(get_pre_hatcher_prompt).put(set_pre_hatcher_prompt),
        )
        .route(
            "/api/settings/default-model",
            get(get_default_model).put(set_default_model),
        )
        .route("/api/settings/providers", get(get_providers))
        .route("/api/settings/providers/{id}", put(set_provider_hidden))
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

/// Whether the Claude permission-bypass escape hatch is on (default off =
/// enforced). Read at dispatch time by
/// `SessionManager::send_message_locked` for spawns that leave
/// `SpawnConfig::permission_mode` unset.
pub(crate) async fn claude_bypass_permissions_for_db(db: Db) -> bool {
    let raw = tokio::task::spawn_blocking(move || {
        db.plugin_store_get_blocking(SETTINGS_NS, SETTINGS_COLLECTION, CLAUDE_BYPASS_KEY)
    })
    .await;
    match raw {
        Ok(Ok(Some(json))) => serde_json::from_str::<serde_json::Value>(&json)
            .ok()
            .and_then(|v| v.get("bypass").and_then(|b| b.as_bool()))
            .unwrap_or(false),
        _ => false,
    }
}

/// GET /api/settings/claude-permissions → `{"bypass": bool}` (default false).
///
/// `bypass = false` (enforced, the default): Claude CLI spawns run with the
/// stdio permission tool, so every tool call not pre-approved via
/// `--allowedTools` round-trips through Peckboard's sandbox gate
/// (project-folder containment + terminal-tool deny). `bypass = true`
/// restores the legacy `--dangerously-skip-permissions` behavior host-wide.
async fn get_claude_permissions(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let bypass = claude_bypass_permissions_for_db(state.db.clone()).await;
    Json(serde_json::json!({ "bypass": bypass }))
}

#[derive(serde::Deserialize)]
struct ClaudePermissionsBody {
    bypass: bool,
}

/// PUT /api/settings/claude-permissions `{"bypass": bool}` → 204. Takes
/// effect on each session's next dispatched spawn — a running CLI child
/// keeps its spawn-time mode until it exits and respawns.
async fn set_claude_permissions(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ClaudePermissionsBody>,
) -> impl IntoResponse {
    let db = state.db.clone();
    let value = serde_json::json!({ "bypass": body.bypass }).to_string();
    let res = tokio::task::spawn_blocking(move || {
        db.plugin_store_put_blocking(SETTINGS_NS, SETTINGS_COLLECTION, CLAUDE_BYPASS_KEY, &value)
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

/// The app-wide default model, or `None` when unset/empty/auto. The dispatch
/// path reads the same key straight off the blocking store
/// (`provider::manager::send_message_locked`); this helper serves the route.
async fn default_model_setting(state: &Arc<AppState>) -> Option<String> {
    let db = state.db.clone();
    let raw = tokio::task::spawn_blocking(move || {
        db.plugin_store_get_blocking(SETTINGS_NS, SETTINGS_COLLECTION, DEFAULT_MODEL_KEY)
    })
    .await;
    match raw {
        Ok(Ok(Some(json))) => serde_json::from_str::<serde_json::Value>(&json)
            .ok()
            .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(str::to_string))
            .filter(|m| !crate::provider::is_auto_model(m)),
        _ => None,
    }
}

/// GET /api/settings/default-model → `{"model": "provider:model" | ""}`
/// ("" = unset: dispatch falls back to effort-based routing).
async fn get_default_model(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let model = default_model_setting(&state).await.unwrap_or_default();
    Json(serde_json::json!({ "model": model }))
}

#[derive(serde::Deserialize)]
struct DefaultModelBody {
    model: String,
}

/// PUT /api/settings/default-model `{"model": "provider:model" | ""}` → 204.
/// Empty clears the setting. Applies to every session, card, and review
/// dispatched without an explicit model, from its next turn.
async fn set_default_model(
    State(state): State<Arc<AppState>>,
    Json(body): Json<DefaultModelBody>,
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
        db.plugin_store_put_blocking(SETTINGS_NS, SETTINGS_COLLECTION, DEFAULT_MODEL_KEY, &value)
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

/// Data-retention limits enforced by the periodic sweeper
/// (`service::retention`). Every field is a count/day bound; `0` means
/// "keep forever" — the default, so upgrading to this feature never starts
/// deleting existing data until an admin opts in.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct RetentionSettings {
    /// Delete a repeating-task run session once it's older than this many
    /// days (by `last_activity`). 0 = no age bound.
    #[serde(default)]
    pub repeating_session_max_age_days: u32,
    /// Per repeating task, keep only the newest N run sessions. 0 = no
    /// count bound.
    #[serde(default)]
    pub repeating_session_max_per_task: u32,
    /// For non-worker sessions idle longer than this many days, delete
    /// their events older than the same cutoff. 0 = no age bound.
    #[serde(default)]
    pub event_max_age_days: u32,
    /// For any non-worker session, keep only the newest N events. 0 = no
    /// count bound.
    #[serde(default)]
    pub event_max_count_per_session: u32,
    /// Delete report files older than this many days (by mtime). 0 = no
    /// age bound.
    #[serde(default)]
    pub report_max_age_days: u32,
    /// Keep only the newest N report files overall. 0 = no count bound.
    #[serde(default)]
    pub report_max_count: u32,
}

impl RetentionSettings {
    const MAX_BOUND: u32 = 36_500; // 100 years / 100k rows-ish — guards against fat-fingered input, not a real limit

    fn validate(&self) -> Result<(), &'static str> {
        let fields = [
            self.repeating_session_max_age_days,
            self.repeating_session_max_per_task,
            self.event_max_age_days,
            self.event_max_count_per_session,
            self.report_max_age_days,
            self.report_max_count,
        ];
        if fields.iter().any(|&v| v > Self::MAX_BOUND) {
            return Err("retention values must be 36500 or less");
        }
        Ok(())
    }
}

/// Read the retention settings from the plugin store. Defaults to
/// [`RetentionSettings::default`] (everything 0 = keep forever) on
/// missing/unparseable data.
pub async fn retention_settings(state: &AppState) -> RetentionSettings {
    let db = state.db.clone();
    let raw = tokio::task::spawn_blocking(move || {
        db.plugin_store_get_blocking(SETTINGS_NS, SETTINGS_COLLECTION, RETENTION_KEY)
    })
    .await;
    match raw {
        Ok(Ok(Some(json))) => serde_json::from_str(&json).unwrap_or_default(),
        _ => RetentionSettings::default(),
    }
}

/// GET /api/settings/retention → [`RetentionSettings`] as JSON.
async fn get_retention(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(retention_settings(&state).await)
}

/// PUT /api/settings/retention → 204. Applies on the sweeper's next hourly
/// tick (it reloads settings every pass), not immediately.
async fn set_retention(
    State(state): State<Arc<AppState>>,
    Json(body): Json<RetentionSettings>,
) -> impl IntoResponse {
    if let Err(msg) = body.validate() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": msg })),
        ));
    }
    let db = state.db.clone();
    let value = serde_json::to_string(&body).unwrap();
    let res = tokio::task::spawn_blocking(move || {
        db.plugin_store_put_blocking(SETTINGS_NS, SETTINGS_COLLECTION, RETENTION_KEY, &value)
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
/// Whether the built-in `mock` provider (used by e2e's `mock:*` models)
/// should appear in the Providers & Accounts settings UI. It's always
/// registered and always reachable via `/api/models` — gating here only
/// hides the toggle from production users, it never affects registration
/// or model dispatch.
fn dev_providers_visible() -> bool {
    cfg!(debug_assertions) || std::env::var("PECKBOARD_SHOW_MOCK_PROVIDER").is_ok_and(|v| v == "1")
}

async fn get_providers(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let hidden = hidden_providers(&state).await;
    let show_mock = dev_providers_visible();
    let mut providers = state.provider_registry.list_providers().await;
    providers.retain(|p| show_mock || p.id != "mock");
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

/// The TLS blob every route below answers with: what the HTTPS listener is
/// serving right now, plus the port it listens on. `source` is spelled for
/// the UI (`self-signed`), not with [`TlsSource`]'s snake_case serde name.
fn tls_status_json(state: &AppState) -> serde_json::Value {
    let status = state.tls.snapshot();
    let source = match status.source {
        Some(TlsSource::Uploaded) => "uploaded",
        Some(TlsSource::SelfSigned) => "self-signed",
        None => "none",
    };
    serde_json::json!({
        "https_enabled": status.https_enabled,
        "https_port": state.config.https_port,
        "source": source,
        "sans": status.sans,
        "not_after": status.not_after,
        "error": status.last_error,
    })
}

/// GET /api/settings/tls → the current TLS status.
async fn get_tls(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(tls_status_json(&state))
}

#[derive(serde::Deserialize)]
struct TlsCertBody {
    cert_pem: String,
    key_pem: String,
}

/// Reject anything `rustls` would reject at handshake time: a malformed
/// PEM, a missing private key, or a key that doesn't match the leaf —
/// `with_single_cert` compares the two. We build against an explicit
/// provider rather than the process default so validation doesn't depend
/// on `main` having installed one.
///
/// This runs before anything touches the disk, so a bad upload can never
/// displace working material.
fn validate_cert_pair(cert_pem: &str, key_pem: &str) -> Result<(), String> {
    let certs = rustls_pemfile::certs(&mut std::io::BufReader::new(cert_pem.as_bytes()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("failed to parse certificate PEM: {e}"))?;
    if certs.is_empty() {
        return Err("no certificate found in the certificate PEM".to_string());
    }

    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(key_pem.as_bytes()))
        .map_err(|e| format!("failed to parse private key PEM: {e}"))?
        .ok_or_else(|| "no private key found in the key PEM".to_string())?;

    rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| format!("failed to build a TLS config: {e}"))?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .map_err(|e| format!("certificate and private key do not work together: {e}"))?;

    Ok(())
}

/// Shared tail for the three mutating TLS routes. On success the startup
/// failure banner is cleared and `https_enabled` is recomputed: the HTTPS
/// listener is bound once at boot and serves whatever the resolver hands
/// out, so a cert that loads now means HTTPS works now — as long as the
/// socket bound in the first place.
async fn finish_tls_change(
    state: &Arc<AppState>,
    res: Result<anyhow::Result<()>, tokio::task::JoinError>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let err = |e: String| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e })),
        )
    };
    match res {
        Ok(Ok(())) => {
            state
                .tls
                .set_https_enabled(state.tls.snapshot().listener_bound);
            if let Err(e) = tls::clear_failure_announcement(&state.db).await {
                tracing::warn!("Failed to clear TLS failure announcement: {e}");
            }
            Ok(Json(tls_status_json(state)))
        }
        Ok(Err(e)) => Err(err(format!("{e:#}"))),
        Err(e) => Err(err(e.to_string())),
    }
}

/// POST /api/settings/tls/cert → install an operator-supplied pair and
/// hot-swap it in. 400 if the pair doesn't validate.
async fn upload_tls_cert(
    State(state): State<Arc<AppState>>,
    Json(body): Json<TlsCertBody>,
) -> impl IntoResponse {
    if let Err(msg) = validate_cert_pair(&body.cert_pem, &body.key_pem) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": msg })),
        ));
    }

    let data_dir = state.config.data_dir.clone();
    let tls_state = state.tls.clone();
    let res = tokio::task::spawn_blocking(move || {
        let material = tls::install_uploaded(&data_dir, &body.cert_pem, &body.key_pem)?;
        tls_state.load_from(&data_dir, &material)
    })
    .await;
    finish_tls_change(&state, res).await
}

/// DELETE /api/settings/tls/cert → drop the uploaded pair and fall back to
/// the self-signed cert, generating one if none is on disk.
async fn delete_tls_cert(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let data_dir = state.config.data_dir.clone();
    let tls_state = state.tls.clone();
    let res = tokio::task::spawn_blocking(move || {
        tls::clear_uploaded(&data_dir)?;
        let material = tls::ensure_certs(&data_dir)?;
        tls_state.load_from(&data_dir, &material)
    })
    .await;
    finish_tls_change(&state, res).await
}

/// POST /api/settings/tls/regenerate → issue a fresh self-signed cert over
/// the SANs this host answers on *now*, so addresses gained since boot are
/// covered. Uploaded material still wins if the operator has some
/// installed: this refreshes the fallback, it does not revert to it (that's
/// `DELETE /api/settings/tls/cert`).
async fn regenerate_tls_cert(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let data_dir = state.config.data_dir.clone();
    let tls_state = state.tls.clone();
    let res = tokio::task::spawn_blocking(move || {
        tls::regenerate_self_signed(&data_dir)?;
        let material = tls::ensure_certs(&data_dir)?;
        tls_state.load_from(&data_dir, &material)
    })
    .await;
    finish_tls_change(&state, res).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::middleware::tests::{seed_authenticated_user, test_state};
    use axum::body::Body;
    use axum::http::{Request, header};
    use tower::ServiceExt;

    fn app(state: Arc<AppState>) -> Router {
        Router::new().merge(router(state.clone())).with_state(state)
    }

    /// A throwaway cert/key pair standing in for operator-supplied material.
    fn generate_pem_pair(dns: &str) -> (String, String) {
        let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let mut params = rcgen::CertificateParams::default();
        params.subject_alt_names = vec![rcgen::SanType::DnsName(dns.try_into().unwrap())];
        let cert = params.self_signed(&key_pair).unwrap();
        (cert.pem(), key_pair.serialize_pem())
    }

    async fn call(
        state: &Arc<AppState>,
        token: &str,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"));
        let req = match body {
            Some(v) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let resp = app(state.clone()).oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    /// The TLS routes read and replace the key material the whole host is
    /// served with, so an admin-created non-admin must not reach any of
    /// them — not even the read.
    #[tokio::test]
    async fn non_admin_cannot_reach_the_tls_routes() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "user").await;
        let (cert_pem, key_pem) = generate_pem_pair("uploaded.example");

        for (method, uri, body) in [
            ("GET", "/api/settings/tls", None),
            (
                "POST",
                "/api/settings/tls/cert",
                Some(serde_json::json!({ "cert_pem": cert_pem, "key_pem": key_pem })),
            ),
            ("DELETE", "/api/settings/tls/cert", None),
            ("POST", "/api/settings/tls/regenerate", None),
        ] {
            let (status, _) = call(&state, &token, method, uri, body).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
        }
        assert!(
            !dir.path().join("certs").exists(),
            "a rejected request must not write any cert material"
        );
    }

    #[tokio::test]
    async fn admin_reads_the_tls_status() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "admin").await;

        let (status, body) = call(&state, &token, "GET", "/api/settings/tls", None).await;
        assert_eq!(status, StatusCode::OK);
        // Nothing loaded in a bare test state.
        assert_eq!(body["source"], "none");
        assert_eq!(body["https_enabled"], false);
        assert_eq!(body["https_port"], 0);
        assert_eq!(body["error"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn a_malformed_upload_is_rejected_before_anything_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "admin").await;

        let (status, body) = call(
            &state,
            &token,
            "POST",
            "/api/settings/tls/cert",
            Some(serde_json::json!({ "cert_pem": "not a pem", "key_pem": "nor is this" })),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"].as_str().unwrap().contains("no certificate"),
            "got {body}"
        );
        assert!(!dir.path().join("certs/uploaded-cert.pem").exists());
    }

    #[tokio::test]
    async fn a_certificate_without_its_key_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "admin").await;
        let (cert_pem, _) = generate_pem_pair("uploaded.example");

        let (status, body) = call(
            &state,
            &token,
            "POST",
            "/api/settings/tls/cert",
            Some(serde_json::json!({ "cert_pem": cert_pem, "key_pem": "" })),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"].as_str().unwrap().contains("no private key"),
            "got {body}"
        );
        assert!(!dir.path().join("certs/uploaded-cert.pem").exists());
    }

    /// Both halves parse, but they're from different pairs — the gate is
    /// `with_single_cert`, which compares the key against the leaf.
    #[tokio::test]
    async fn a_key_that_does_not_match_the_certificate_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "admin").await;
        let (cert_pem, _) = generate_pem_pair("one.example");
        let (_, key_pem) = generate_pem_pair("two.example");

        let (status, body) = call(
            &state,
            &token,
            "POST",
            "/api/settings/tls/cert",
            Some(serde_json::json!({ "cert_pem": cert_pem, "key_pem": key_pem })),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST, "got {body}");
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("do not work together"),
            "got {body}"
        );
        assert!(!dir.path().join("certs/uploaded-cert.pem").exists());
    }

    #[tokio::test]
    async fn admin_upload_hot_swaps_and_reports_uploaded() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "admin").await;
        let (cert_pem, key_pem) = generate_pem_pair("uploaded.example");

        let (status, body) = call(
            &state,
            &token,
            "POST",
            "/api/settings/tls/cert",
            Some(serde_json::json!({ "cert_pem": cert_pem, "key_pem": key_pem })),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "got {body}");
        assert_eq!(body["source"], "uploaded");
        assert_eq!(body["error"], serde_json::Value::Null);
        assert!(body["not_after"].is_string(), "got {body}");
        assert!(dir.path().join("certs/uploaded-cert.pem").exists());
        assert!(dir.path().join("certs/uploaded-key.pem").exists());

        // The status route agrees, so the swap really landed in TlsState.
        let (_, again) = call(&state, &token, "GET", "/api/settings/tls", None).await;
        assert_eq!(again["source"], "uploaded");
    }

    #[tokio::test]
    async fn deleting_the_uploaded_certificate_reverts_to_self_signed() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "admin").await;
        let (cert_pem, key_pem) = generate_pem_pair("uploaded.example");

        let (status, _) = call(
            &state,
            &token,
            "POST",
            "/api/settings/tls/cert",
            Some(serde_json::json!({ "cert_pem": cert_pem, "key_pem": key_pem })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = call(&state, &token, "DELETE", "/api/settings/tls/cert", None).await;
        assert_eq!(status, StatusCode::OK, "got {body}");
        assert_eq!(body["source"], "self-signed");
        assert!(
            !body["sans"].as_array().unwrap().is_empty(),
            "self-signed material reports its SANs: {body}"
        );
        assert!(!dir.path().join("certs/uploaded-cert.pem").exists());
        assert!(!dir.path().join("certs/uploaded-key.pem").exists());
    }

    #[tokio::test]
    async fn regenerate_issues_a_fresh_self_signed_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "admin").await;
        let cert_path = dir.path().join("certs/cert.pem");

        let (status, body) =
            call(&state, &token, "POST", "/api/settings/tls/regenerate", None).await;
        assert_eq!(status, StatusCode::OK, "got {body}");
        assert_eq!(body["source"], "self-signed");
        let first = std::fs::read_to_string(&cert_path).unwrap();

        // A still-valid cert doesn't short-circuit the route the way
        // `ensure_certs` would — this is the "pick up a new IP" path.
        let (status, _) = call(&state, &token, "POST", "/api/settings/tls/regenerate", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_ne!(
            first,
            std::fs::read_to_string(&cert_path).unwrap(),
            "regenerate must mint new material, not reuse the current cert"
        );
    }

    /// Regenerate refreshes the self-signed fallback; it is not a way to
    /// silently drop an operator's uploaded certificate.
    #[tokio::test]
    async fn regenerate_keeps_serving_uploaded_material() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "admin").await;
        let (cert_pem, key_pem) = generate_pem_pair("uploaded.example");

        let (status, _) = call(
            &state,
            &token,
            "POST",
            "/api/settings/tls/cert",
            Some(serde_json::json!({ "cert_pem": cert_pem, "key_pem": key_pem })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) =
            call(&state, &token, "POST", "/api/settings/tls/regenerate", None).await;
        assert_eq!(status, StatusCode::OK, "got {body}");
        assert_eq!(body["source"], "uploaded");
        assert!(
            dir.path().join("certs/cert.pem").exists(),
            "the self-signed fallback is refreshed even while uploaded material serves"
        );
    }
}
