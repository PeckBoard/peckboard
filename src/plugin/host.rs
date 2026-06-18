//! Data-access host functions exposed to WASM plugins.
//!
//! WASM plugins run fully sandboxed — no filesystem, no network. The only
//! way they can read or write Peckboard data is by calling back through the
//! host functions registered here, which are wired into every loaded plugin
//! in [`crate::plugin::manager::PluginManager`].
//!
//! Each function is JSON-string-in / JSON-string-out and **must never panic
//! across the FFI boundary** — every error path returns an `{"error": ...}`
//! JSON object instead of unwinding. The extism `host_fn!` macro handles the
//! plugin-memory marshalling; the real logic lives in the `*_impl` free
//! functions, which are synchronous (so they can run inside the synchronous
//! extism call without entering the async runtime) and unit-testable on their
//! own with [`crate::db::Db::in_memory`].
//!
//! The original data-access functions (projects/cards/plugin-settings) are
//! intentionally generic and **not** permission-gated: every loaded `.wasm`
//! plugin can call them, including the `peckboard_create_card` write —
//! anything dropped into `<dataDir>/plugins/` is already trusted to run
//! in-process. The newer *capability* functions are different: the
//! `peckboard_store_*`, `peckboard_session_meta_*`, `peckboard_*_session`,
//! `peckboard_append_event`, and `peckboard_list_project_files` /
//! `peckboard_read_file` family each require the plugin to hold the matching
//! manifest permission (`data_store`, `session_read`/`session_write`,
//! `event_append`, `project_files_read`) — checked at call time against the
//! granted set ([`HostState::permissions`]) — and the session/event/file
//! functions additionally re-derive the caller's scope from the trusted
//! [`InvocationContext`] (never plugin-supplied ids) before touching shared
//! session data or reading the caller's folder.
//!
//! The plugin-settings functions are the exception that proves the rule: they
//! are *namespaced* to the calling plugin. Each loaded plugin gets its own
//! host-function set carrying its own id ([`HostState::plugin_id`]), so a
//! plugin can only read and write rows under its own `plugin_id` — it cannot
//! reach another plugin's stored state. The stored values are returned to the
//! owning plugin verbatim (it is the data's owner and needs the real value,
//! e.g. to verify an API key); redaction of secrets only happens at the
//! separate `/api/plugins/:id/settings` HTTP surface, which surfaces values to
//! the browser. These host functions never log stored values.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use extism::*;
use serde::Deserialize;

use crate::db::Db;
use crate::db::models::NewCard;

/// Per-plugin user data shared by all of a single plugin's host functions.
///
/// Carries the live [`Db`] handle plus the **calling plugin's id**, so the
/// plugin-settings functions can scope every read/write to that plugin's own
/// namespace. Each loaded plugin is wired with its own `HostState` (see
/// [`host_functions`]); they are not shared across plugins.
struct HostState {
    db: Db,
    plugin_id: String,
    /// The plugin's granted host permissions. Shared (and populated) by the
    /// loader after it parses the manifest — host functions are wired before
    /// the manifest is known, so this starts empty and is filled in before
    /// the plugin can run any code that could call a gated function. Because
    /// a plugin is inert until its full grant is approved, whenever a host
    /// function actually runs this holds exactly the declared permission set.
    permissions: Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    /// The **trusted** context of the MCP-tool invocation currently running in
    /// this plugin, set by [`PluginManager::invoke_mcp_tool`] from the verified
    /// caller `ToolCallContext` (the MCP token + session row) immediately before
    /// it calls the plugin's `handle`, and cleared to `None` afterward. Scoped
    /// host functions read it to re-derive the caller's session / project /
    /// folder server-side — they MUST NOT trust ids the plugin passes as
    /// arguments, or a plugin tool could reach another folder or project
    /// (DESIGN §7.4). `None` outside an `mcp.tool.invoke` dispatch (e.g. during
    /// `init` or an ordinary hook), so those scoped functions refuse.
    invocation: Arc<std::sync::RwLock<Option<InvocationContext>>>,
    /// Late-bound bridge to live-application capabilities (agent dispatch)
    /// that need the running `AppState`, not just the `Db`. Shared by every
    /// plugin and set once by `main.rs` after `AppState` is built (see
    /// [`crate::plugin::manager::PluginManager::set_live_host`]); `None` until
    /// then and for managers that host no app (tests), so the live host
    /// functions refuse rather than act.
    live: Arc<std::sync::RwLock<Option<Arc<dyn LiveHost>>>>,
    /// The **trusted** authenticated-user context of an in-flight plugin UI
    /// request, set by [`crate::plugin::manager::PluginManager::serve_http_authed`]
    /// around the `http.request.authed` dispatch and cleared afterward. Its
    /// presence lets the scoped host functions act under the user's authority
    /// (gated by the `user_authority` permission). `None` outside an
    /// authenticated request.
    user: Arc<std::sync::RwLock<Option<UserContext>>>,
}

/// Live-application capabilities a plugin host function may invoke that need
/// the running `AppState` (agent dispatch), beyond what the `Db` alone offers.
/// Defined here so the plugin layer stays free of any `AppState` coupling; the
/// concrete impl (`AppLiveHost`) lives above it and is late-bound into the
/// manager once the app exists. **Every method is fire-and-forget** — it
/// schedules work on the async runtime and returns immediately, so a
/// synchronous WASM `handle` call never blocks on an agent run (respecting the
/// call timeout). Authorization/scope is enforced by the caller *before* these
/// run; the impl just performs the already-checked action.
pub trait LiveHost: Send + Sync {
    /// Force a fresh capture run on `session_id` with `prompt` (maps to
    /// `ExpertDispatcher::dispatch_capture`).
    fn dispatch_capture(&self, session_id: String, prompt: String);
    /// Deliver `text` to `session_id` and resume it — spawn if idle, queue /
    /// inject if running (maps to `ExpertDispatcher::resume_session`).
    fn resume_session(&self, session_id: String, text: String);
}

/// The verified caller scope of an in-flight `mcp.tool.invoke` — the keys the
/// host checks to keep a plugin tool inside the caller's reach. Deserialized
/// from the same context slice `routes/mcp.rs` hands the plugin, but set
/// host-side (never plugin-supplied) so scope checks can trust it. Only the
/// scope keys live here; the plugin separately receives the full context
/// (incl. `sessionId`/`cardId`) in its invoke payload. `project_id` is `None`
/// for an unscoped chat caller; `folder_id` is `None` only if the caller's
/// session somehow lacks a folder (then scoped writes refuse).
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct InvocationContext {
    #[serde(rename = "projectId", default)]
    pub project_id: Option<String>,
    #[serde(rename = "folderId", default)]
    pub folder_id: Option<String>,
    /// `true` when the caller is an **authenticated user** acting through the
    /// plugin's UI (set host-side by `serve_http_authed`, never deserialized),
    /// not an MCP tool invocation. Under user authority the session/dispatch
    /// scope checks pass for any session (the user has full app authority, like
    /// core's own authenticated `/api/*` routes), while the per-folder/project
    /// visibility floor still applies to MCP tool calls (`authority == false`).
    #[serde(skip, default)]
    pub authority: bool,
}

/// The trusted context of an authenticated, user-facing plugin request — set by
/// [`crate::plugin::manager::PluginManager::serve_http_authed`] from the
/// `require_auth`-verified user for exactly the span of the plugin call, then
/// cleared. Carries the user id (for audit / future per-user scoping); its mere
/// presence authorizes the scoped host functions to act under the user's
/// authority. `None` outside an authenticated request.
#[derive(Clone, Debug)]
pub(crate) struct UserContext {
    #[allow(dead_code)] // carried for audit / future per-user scoping
    pub user_id: String,
}

impl UserContext {
    /// The caller context a host function sees for an authenticated user
    /// request: full authority, no folder/project floor.
    fn as_invocation(&self) -> InvocationContext {
        InvocationContext {
            project_id: None,
            folder_id: None,
            authority: true,
        }
    }
}

/// JSON request for `peckboard_list_cards`. All fields optional; a missing
/// `project_id` lists cards across every project, and `step` filters the
/// result to a single workflow step.
#[derive(Deserialize)]
struct ListCardsRequest {
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    step: Option<String>,
}

/// JSON request for `peckboard_create_card`.
#[derive(Deserialize)]
struct CreateCardRequest {
    project_id: String,
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    step: Option<String>,
    #[serde(default)]
    priority: Option<i32>,
    #[serde(default)]
    workflow: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    blocked: Option<bool>,
    #[serde(default)]
    block_reason: Option<String>,
}

/// JSON request for `peckboard_get_plugin_setting` and (the key half of)
/// `peckboard_set_plugin_setting`.
#[derive(Deserialize)]
struct GetPluginSettingRequest {
    key: String,
}

/// JSON request for `peckboard_set_plugin_setting`. A missing or `null`
/// `value` deletes the key (matching the `set_plugin_settings_batch`
/// convention), so the schema default — if any — takes over.
#[derive(Deserialize)]
struct SetPluginSettingRequest {
    key: String,
    #[serde(default)]
    value: serde_json::Value,
}

/// Largest setting key Peckboard will accept from a plugin. Keeps a
/// misbehaving plugin from filling the `plugin_settings` table with
/// pathological keys; comfortably larger than any real key name.
const MAX_SETTING_KEY_LEN: usize = 256;

/// Largest serialized setting value (in bytes) a plugin may store. API
/// keys and small JSON blobs are tiny; this caps a runaway plugin without
/// constraining legitimate use.
const MAX_SETTING_VALUE_LEN: usize = 64 * 1024;

/// The JSON error envelope every host function returns on failure.
fn error_json(msg: impl std::fmt::Display) -> String {
    serde_json::json!({ "error": msg.to_string() }).to_string()
}

/// Reject empty / oversized setting keys before they reach the DB.
fn validate_setting_key(key: &str) -> Result<(), String> {
    let key = key.trim();
    if key.is_empty() {
        return Err("key is required".to_string());
    }
    if key.len() > MAX_SETTING_KEY_LEN {
        return Err(format!(
            "key too long: {} bytes (max {MAX_SETTING_KEY_LEN})",
            key.len()
        ));
    }
    Ok(())
}

/// `peckboard_list_projects` — list every project (read).
pub(crate) fn list_projects_impl(db: &Db) -> String {
    match db.list_projects_blocking() {
        Ok(projects) => serde_json::json!({ "projects": projects }).to_string(),
        Err(e) => error_json(e),
    }
}

/// `peckboard_list_cards` — list cards, optionally filtered by project and
/// step (read).
pub(crate) fn list_cards_impl(db: &Db, input: &str) -> String {
    let req: ListCardsRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };

    match db.list_cards_blocking(req.project_id.as_deref()) {
        Ok(mut cards) => {
            if let Some(step) = req.step.as_deref() {
                cards.retain(|c| c.step == step);
            }
            serde_json::json!({ "cards": cards }).to_string()
        }
        Err(e) => error_json(e),
    }
}

/// `peckboard_create_card` — create a card on a project (write).
///
/// Mirrors the validation the HTTP route does (priority in the allowed set,
/// project must exist, explicit workflow ids validated, workflow inherited
/// from the project otherwise) but does NOT fire the `card.create.before`
/// hook or broadcast — it is the generic data primitive; policy lives in the
/// calling plugin.
pub(crate) fn create_card_impl(db: &Db, input: &str) -> String {
    let req: CreateCardRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };

    let title = req.title.trim();
    if title.is_empty() {
        return error_json("title is required");
    }

    let priority = req.priority.unwrap_or(2);
    if !crate::routes::misc::is_valid_priority(priority) {
        return error_json(format!(
            "invalid priority: {priority} (allowed: 0=Critical, 1=High, 2=Medium, 3=Low)"
        ));
    }

    // Project must exist; we also need its workflow as the inherited default.
    let project = match db.get_project_blocking(&req.project_id) {
        Ok(Some(p)) => p,
        Ok(None) => return error_json("project not found"),
        Err(e) => return error_json(e),
    };

    // Resolve the card's workflow: validate an explicit non-empty id against
    // the registry, otherwise copy the project's.
    let workflow = match req.workflow.as_deref().map(str::trim) {
        Some(id) if !id.is_empty() => {
            if crate::workflow::workflow_by_id(id).is_none() {
                return error_json(format!("unknown workflow id '{id}'"));
            }
            id.to_string()
        }
        _ => project.workflow.clone(),
    };

    // A non-empty block_reason implicitly blocks the card, matching the route.
    let block_reason = req
        .block_reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let blocked = req.blocked.unwrap_or(block_reason.is_some());

    let step = req
        .step
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("backlog")
        .to_string();

    let now = chrono::Utc::now().to_rfc3339();
    let new = NewCard {
        id: uuid::Uuid::new_v4().to_string(),
        project_id: req.project_id.clone(),
        title: title.to_string(),
        description: req.description.unwrap_or_default(),
        step,
        priority,
        workflow,
        model: req.model,
        effort: req.effort,
        blocked,
        block_reason,
        created_at: now.clone(),
        updated_at: now,
    };

    match db.create_card_blocking(&new) {
        Ok(card) => serde_json::json!({ "card": card }).to_string(),
        Err(e) => error_json(e),
    }
}

/// `peckboard_get_plugin_setting` — read one of the calling plugin's own
/// stored settings (read, namespaced to `plugin_id`).
///
/// Returns the value verbatim — the calling plugin owns this data and needs
/// the real value (e.g. to verify an API key it stored). `{"value": null}`
/// when the key is unset. Never logs the value.
pub(crate) fn get_plugin_setting_impl(db: &Db, plugin_id: &str, input: &str) -> String {
    let req: GetPluginSettingRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    if let Err(e) = validate_setting_key(&req.key) {
        return error_json(e);
    }
    match db.get_plugin_setting_blocking(plugin_id, req.key.trim()) {
        Ok(value) => serde_json::json!({ "value": value }).to_string(),
        Err(e) => error_json(e),
    }
}

/// `peckboard_set_plugin_setting` — write one of the calling plugin's own
/// stored settings (write, namespaced to `plugin_id`).
///
/// A `null` (or omitted) value deletes the key. Rejects oversized
/// keys/values so a misbehaving plugin can't bloat the table. Returns
/// `{"ok": true}` on success. Never logs the value.
pub(crate) fn set_plugin_setting_impl(db: &Db, plugin_id: &str, input: &str) -> String {
    let req: SetPluginSettingRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    if let Err(e) = validate_setting_key(&req.key) {
        return error_json(e);
    }
    // Bound the stored value. `serde_json::to_string` only fails on
    // non-serializable values, which a parsed `Value` never is.
    if let Ok(encoded) = serde_json::to_string(&req.value)
        && encoded.len() > MAX_SETTING_VALUE_LEN
    {
        return error_json(format!(
            "value too large: {} bytes (max {MAX_SETTING_VALUE_LEN})",
            encoded.len()
        ));
    }
    match db.set_plugin_setting_blocking(plugin_id, req.key.trim(), &req.value) {
        Ok(()) => serde_json::json!({ "ok": true }).to_string(),
        Err(e) => error_json(e),
    }
}

/// `peckboard_list_plugin_settings` — list all of the calling plugin's own
/// stored settings as a `key → value` object (read, namespaced to
/// `plugin_id`). Values are returned verbatim; never logs them.
pub(crate) fn list_plugin_settings_impl(db: &Db, plugin_id: &str) -> String {
    match db.list_plugin_settings_blocking(plugin_id) {
        Ok(settings) => serde_json::json!({ "settings": settings }).to_string(),
        Err(e) => error_json(e),
    }
}

// ── Generic plugin storage host functions (Phase A / A4) ──────────────
//
// All gated: a plugin without the matching permission gets an `{"error":..}`.
// `data` fields are arbitrary JSON the plugin owns; core stores them verbatim
// and never queries into them.

/// Max serialized size of a stored document / session-meta blob (256 KiB).
const PLUGIN_DOC_MAX_BYTES: usize = 256 * 1024;

#[derive(Deserialize)]
struct StorePutRequest {
    collection: String,
    key: String,
    data: serde_json::Value,
}

#[derive(Deserialize)]
struct StoreKeyRequest {
    collection: String,
    key: String,
}

#[derive(Deserialize)]
struct StoreListRequest {
    collection: String,
}

#[derive(Deserialize)]
struct SessionMetaSetRequest {
    session_id: String,
    data: serde_json::Value,
}

#[derive(Deserialize)]
struct SessionMetaGetRequest {
    session_id: String,
}

/// Reject empty / oversized identifiers so a misbehaving plugin can't bloat
/// the key space.
fn validate_id(label: &str, value: &str) -> Result<(), String> {
    let v = value.trim();
    if v.is_empty() {
        return Err(format!("{label} must not be empty"));
    }
    if v.len() > 256 {
        return Err(format!("{label} exceeds 256 bytes"));
    }
    Ok(())
}

/// Serialize a plugin-supplied JSON value for storage, enforcing the size cap.
fn encode_doc(data: &serde_json::Value) -> Result<String, String> {
    let s = serde_json::to_string(data).map_err(|e| format!("invalid data: {e}"))?;
    if s.len() > PLUGIN_DOC_MAX_BYTES {
        return Err(format!("data exceeds {PLUGIN_DOC_MAX_BYTES} bytes"));
    }
    Ok(s)
}

/// Parse a stored raw document back to JSON; a row whose JSON has rotted
/// surfaces as `null` rather than failing the read.
fn decode_doc(raw: Option<String>) -> serde_json::Value {
    match raw {
        Some(s) => serde_json::from_str(&s).unwrap_or(serde_json::Value::Null),
        None => serde_json::Value::Null,
    }
}

pub(crate) fn store_put_impl(db: &Db, plugin_id: &str, input: &str) -> String {
    let req: StorePutRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    for (label, v) in [("collection", &req.collection), ("key", &req.key)] {
        if let Err(e) = validate_id(label, v) {
            return error_json(e);
        }
    }
    let data = match encode_doc(&req.data) {
        Ok(d) => d,
        Err(e) => return error_json(e),
    };
    match db.plugin_store_put_blocking(plugin_id, req.collection.trim(), req.key.trim(), &data) {
        Ok(()) => serde_json::json!({ "ok": true }).to_string(),
        Err(e) => error_json(e),
    }
}

pub(crate) fn store_get_impl(db: &Db, plugin_id: &str, input: &str) -> String {
    let req: StoreKeyRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    match db.plugin_store_get_blocking(plugin_id, req.collection.trim(), req.key.trim()) {
        Ok(raw) => serde_json::json!({ "value": decode_doc(raw) }).to_string(),
        Err(e) => error_json(e),
    }
}

pub(crate) fn store_list_impl(db: &Db, plugin_id: &str, input: &str) -> String {
    let req: StoreListRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    match db.plugin_store_list_blocking(plugin_id, req.collection.trim()) {
        Ok(rows) => {
            let items: Vec<serde_json::Value> = rows
                .into_iter()
                .map(|(key, raw)| serde_json::json!({ "key": key, "value": decode_doc(Some(raw)) }))
                .collect();
            serde_json::json!({ "items": items }).to_string()
        }
        Err(e) => error_json(e),
    }
}

pub(crate) fn store_delete_impl(db: &Db, plugin_id: &str, input: &str) -> String {
    let req: StoreKeyRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    match db.plugin_store_delete_blocking(plugin_id, req.collection.trim(), req.key.trim()) {
        Ok(deleted) => serde_json::json!({ "deleted": deleted }).to_string(),
        Err(e) => error_json(e),
    }
}

pub(crate) fn session_meta_set_impl(db: &Db, plugin_id: &str, input: &str) -> String {
    let req: SessionMetaSetRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    if let Err(e) = validate_id("session_id", &req.session_id) {
        return error_json(e);
    }
    let data = match encode_doc(&req.data) {
        Ok(d) => d,
        Err(e) => return error_json(e),
    };
    match db.plugin_session_meta_set_blocking(req.session_id.trim(), plugin_id, &data) {
        Ok(()) => serde_json::json!({ "ok": true }).to_string(),
        Err(e) => error_json(e),
    }
}

pub(crate) fn session_meta_get_impl(db: &Db, plugin_id: &str, input: &str) -> String {
    let req: SessionMetaGetRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    match db.plugin_session_meta_get_blocking(req.session_id.trim(), plugin_id) {
        Ok(raw) => serde_json::json!({ "value": decode_doc(raw) }).to_string(),
        Err(e) => error_json(e),
    }
}

// ── Generic session / event host functions (gated, scoped) ────────────
//
// These act on *sessions*, which (unlike the plugin's private store) are
// shared core data — so every one re-derives the caller's scope from the
// trusted [`InvocationContext`] and refuses to step outside it. The boundary
// has two parts, both required:
//   1. **Ownership** — a plugin may only get/update/append-to a session it
//      manages, i.e. one carrying *its own* `plugin_session_meta`. A plugin
//      cannot reach an arbitrary user session it never marked.
//   2. **Caller visibility** — the session must be in the caller's folder, in
//      the caller's project, or global (`project_id` NULL). This is the same
//      hard floor core's MCP scope tokens enforce, re-checked server-side so a
//      plugin-supplied id can't cross a folder/project boundary (DESIGN §7.4).
// `create_session` always lands the new row in the *caller's* folder/project,
// so a plugin can't seed a session into someone else's scope either.
//
// Note (Phase B): core lets a *knowledge* expert be consulted cross-project;
// rule (2) is stricter (no cross-project read unless global). That narrowing is
// intentional for now — safer default — and revisited when the PM/cross-project
// consult policy moves in Phase C.

#[derive(Deserialize)]
struct CreateSessionRequest {
    name: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
}

#[derive(Deserialize)]
struct GetSessionRequest {
    session_id: String,
}

#[derive(Deserialize)]
struct UpdateSessionRequest {
    session_id: String,
    // Only generic, plugin-relevant fields are updatable. Expert-specific
    // state (knowledge summary/area/scope) lives in `plugin_session_meta`, not
    // these dormant core columns, so it is deliberately not exposed here.
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    model: Option<Option<String>>,
    #[serde(default)]
    effort: Option<Option<String>>,
}

#[derive(Deserialize)]
struct ListSessionsRequest {
    /// When true, only sessions in the caller's *own* project (and globals)
    /// are returned; otherwise all sessions the caller may see (its folder,
    /// its project, or global) that this plugin manages.
    #[serde(default)]
    project_only: bool,
}

#[derive(Deserialize)]
struct AppendEventRequest {
    session_id: String,
    kind: String,
    data: serde_json::Value,
}

/// Whether the caller (per its trusted context) may see `session`. An
/// authenticated user (`authority`) sees everything — same as core's own
/// `/api/*` routes. An MCP tool call is held to the hard scope floor: same
/// folder, same project, or a global (`project_id` NULL) session.
fn session_visible_to(session: &crate::db::models::Session, inv: &InvocationContext) -> bool {
    if inv.authority {
        return true; // authenticated user — full app authority
    }
    if session.project_id.is_none() {
        return true; // global session — visible across folders/projects
    }
    if inv.folder_id.as_deref() == Some(session.folder_id.as_str()) {
        return true; // same folder
    }
    inv.project_id.is_some() && inv.project_id == session.project_id
}

/// `peckboard_create_session` — create a generic session in the *caller's*
/// folder and project. Expert-ness etc. is the plugin's own metadata
/// (`peckboard_session_meta_set`), never a core column.
pub(crate) fn create_session_impl(db: &Db, input: &str, inv: &InvocationContext) -> String {
    let req: CreateSessionRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    if req.name.trim().is_empty() {
        return error_json("name is required");
    }
    let Some(folder_id) = inv.folder_id.clone() else {
        return error_json("caller has no folder scope; cannot create a session");
    };
    if let Some(id) = req.id.as_deref()
        && let Err(e) = validate_id("id", id)
    {
        return error_json(e);
    }
    let now = chrono::Utc::now().to_rfc3339();
    let new = crate::db::models::NewSession {
        id: req.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        name: req.name,
        folder_id,
        model: req.model,
        effort: req.effort,
        is_worker: false,
        project_id: inv.project_id.clone(),
        card_id: None,
        conversation_id: None,
        created_at: now.clone(),
        last_activity: now,
        is_expert: false,
        expert_kind: None,
        knowledge_summary: None,
        knowledge_area: None,
        scope_path: None,
        is_permanent: false,
        repeating_task_id: None,
    };
    match db.create_session_blocking(new) {
        Ok(session) => serde_json::json!({ "session": session }).to_string(),
        Err(e) => error_json(e),
    }
}

/// `peckboard_get_session` — read one session the plugin manages and the
/// caller may see.
pub(crate) fn get_session_impl(
    db: &Db,
    plugin_id: &str,
    input: &str,
    inv: &InvocationContext,
) -> String {
    let req: GetSessionRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    match fetch_owned_visible_session(db, plugin_id, req.session_id.trim(), inv) {
        Ok(session) => serde_json::json!({ "session": session }).to_string(),
        Err(e) => error_json(e),
    }
}

/// `peckboard_update_session` — update generic fields of a session the plugin
/// manages and the caller may see.
pub(crate) fn update_session_impl(
    db: &Db,
    plugin_id: &str,
    input: &str,
    inv: &InvocationContext,
) -> String {
    let req: UpdateSessionRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    // Authorize against the *current* row before writing.
    if let Err(e) = fetch_owned_visible_session(db, plugin_id, req.session_id.trim(), inv) {
        return error_json(e);
    }
    let update = crate::db::models::UpdateSession {
        name: req.name,
        model: req.model,
        effort: req.effort,
        last_activity: Some(chrono::Utc::now().to_rfc3339()),
        project_id: None,
        card_id: None,
        conversation_id: None,
        is_expert: None,
        expert_kind: None,
        knowledge_summary: None,
        knowledge_area: None,
        scope_path: None,
        is_permanent: None,
    };
    match db.update_session_blocking(req.session_id.trim(), update) {
        Ok(Some(session)) => serde_json::json!({ "session": session }).to_string(),
        Ok(None) => error_json("session not found"),
        Err(e) => error_json(e),
    }
}

/// `peckboard_list_sessions` — list the sessions this plugin manages (carry
/// its `plugin_session_meta`) that the caller may see. Returns each session
/// plus its `meta` blob so the plugin needn't round-trip per id. Sorted by
/// `last_activity` desc.
pub(crate) fn list_sessions_impl(
    db: &Db,
    plugin_id: &str,
    input: &str,
    inv: &InvocationContext,
) -> String {
    let req: ListSessionsRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    let metas = match db.plugin_session_meta_list_blocking(plugin_id) {
        Ok(m) => m,
        Err(e) => return error_json(e),
    };
    let mut out: Vec<serde_json::Value> = Vec::new();
    for (session_id, raw) in metas {
        let Ok(Some(session)) = db.get_session_blocking(&session_id) else {
            continue; // meta orphaned (session gone) — skip
        };
        if !session_visible_to(&session, inv) {
            continue;
        }
        // `project_only` narrows an MCP caller to its own project; it has no
        // meaning for an authenticated user (who sees every project anyway).
        if !inv.authority
            && req.project_only
            && session.project_id.is_some()
            && session.project_id != inv.project_id
        {
            continue;
        }
        out.push(serde_json::json!({
            "session": session,
            "meta": decode_doc(Some(raw)),
        }));
    }
    out.sort_by(|a, b| {
        let la = a["session"]["last_activity"].as_str().unwrap_or("");
        let lb = b["session"]["last_activity"].as_str().unwrap_or("");
        lb.cmp(la)
    });
    serde_json::json!({ "sessions": out }).to_string()
}

/// `peckboard_append_event` — persist one event onto a session the plugin
/// manages and the caller may see (no broadcast; use `peckboard_broadcast`).
pub(crate) fn append_event_impl(
    db: &Db,
    plugin_id: &str,
    input: &str,
    inv: &InvocationContext,
) -> String {
    let req: AppendEventRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    if req.kind.trim().is_empty() {
        return error_json("kind is required");
    }
    if let Err(e) = fetch_owned_visible_session(db, plugin_id, req.session_id.trim(), inv) {
        return error_json(e);
    }
    let data = match encode_doc(&req.data) {
        Ok(d) => d,
        Err(e) => return error_json(e),
    };
    match db.append_event_blocking(req.session_id.trim(), req.kind.trim(), &data) {
        Ok(()) => serde_json::json!({ "ok": true }).to_string(),
        Err(e) => error_json(e),
    }
}

/// Load a session and authorize it for this plugin + caller: it must carry the
/// plugin's `plugin_session_meta` (ownership) *and* be visible to the caller
/// (scope floor). Used by the data functions (get / update / append-event),
/// where a plugin should only reach a session it manages. Returns a uniform
/// `"session not found"` on any failure so a plugin can't probe for sessions
/// outside its reach. See the module note.
fn fetch_owned_visible_session(
    db: &Db,
    plugin_id: &str,
    session_id: &str,
    inv: &InvocationContext,
) -> Result<crate::db::models::Session, String> {
    if session_id.is_empty() {
        return Err("session_id is required".to_string());
    }
    // Ownership: the plugin must have marked this session.
    match db.plugin_session_meta_get_blocking(session_id, plugin_id) {
        Ok(Some(_)) => {}
        Ok(None) => return Err("session not found".to_string()),
        Err(e) => return Err(e.to_string()),
    }
    fetch_visible_session(db, session_id, inv)
}

/// Load a session and authorize it by *visibility only*: it must exist and lie
/// in the caller's folder, project, or be global. Used by the live-dispatch
/// functions (`dispatch_capture` / `resume_session`), which legitimately act on
/// sessions the plugin does NOT own — most importantly delivering an expert's
/// answer back to the *asking* session, exactly as core's own expert delivery
/// does within the folder/project boundary. Ownership is the wrong gate there;
/// the §7.4 boundary that matters is "no cross-folder/project escalation",
/// which `session_visible_to` enforces. Same `"session not found"` framing.
fn fetch_visible_session(
    db: &Db,
    session_id: &str,
    inv: &InvocationContext,
) -> Result<crate::db::models::Session, String> {
    if session_id.is_empty() {
        return Err("session_id is required".to_string());
    }
    let session = match db.get_session_blocking(session_id) {
        Ok(Some(s)) => s,
        Ok(None) => return Err("session not found".to_string()),
        Err(e) => return Err(e.to_string()),
    };
    if !session_visible_to(&session, inv) {
        return Err("session not found".to_string());
    }
    Ok(session)
}

// ── Project file access (gated, scoped to the caller's folder) ────────
//
// A plugin may read only the caller's *folder* directory — the same boundary
// core uses as a session's working dir — resolved from the trusted context,
// never a plugin-supplied id. Listed/accepted paths are relative to that root;
// every read re-resolves against the canonicalized root and refuses anything
// that escapes it (absolute paths, `..`, or a symlink pointing outside). The
// walk uses `file_type()` (lstat), so symlinks are neither followed nor listed
// — no cycle risk and no escape via a linked subtree. Output is bounded so a
// huge tree can't blow the WASM result marshaling.

const PLUGIN_FS_MAX_DEPTH: usize = 8;
const PLUGIN_FS_MAX_FILES: usize = 20_000;
const PLUGIN_FS_MAX_READ_BYTES: usize = 1024 * 1024; // 1 MiB

#[derive(Deserialize)]
struct ReadFileRequest {
    path: String,
}

/// Directories the file walk never descends into — hidden dirs (`.git`, …)
/// plus common build/vendor output. Mirrors the experts handler's
/// `is_ignored_dir` so a plugin's view matches core's codebase scan.
fn is_ignored_fs_dir(name: &str) -> bool {
    if name.starts_with('.') {
        return true;
    }
    matches!(
        name,
        "node_modules"
            | "target"
            | "dist"
            | "build"
            | "vendor"
            | "out"
            | "bin"
            | "obj"
            | "coverage"
            | "__pycache__"
            | "venv"
    )
}

/// Resolve the caller's folder root to a real (canonicalized) directory from
/// the trusted context. Canonicalizing here lets the `read_file` containment
/// check compare real paths and defeats symlink escapes.
fn caller_folder_root(db: &Db, inv: &InvocationContext) -> Result<PathBuf, String> {
    let Some(folder_id) = inv.folder_id.as_deref() else {
        return Err("caller has no folder scope".to_string());
    };
    let folder = match db.get_folder_blocking(folder_id) {
        Ok(Some(f)) => f,
        Ok(None) => return Err("caller folder not found".to_string()),
        Err(e) => return Err(e.to_string()),
    };
    std::fs::canonicalize(&folder.path).map_err(|e| format!("folder path unavailable: {e}"))
}

/// Recursively collect files under `dir` (relative to `root`) with sizes,
/// honoring the depth, ignore, and count caps. Sets `truncated` when the file
/// cap is hit so the caller knows the listing is partial.
fn walk_project_files(
    dir: &Path,
    root: &Path,
    depth: usize,
    out: &mut Vec<serde_json::Value>,
    truncated: &mut bool,
) {
    if depth > PLUGIN_FS_MAX_DEPTH || *truncated {
        return;
    }
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        if out.len() >= PLUGIN_FS_MAX_FILES {
            *truncated = true;
            return;
        }
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        let path = entry.path();
        if file_type.is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            if is_ignored_fs_dir(&name) {
                continue;
            }
            walk_project_files(&path, root, depth + 1, out, truncated);
        } else if file_type.is_file()
            && let Ok(rel) = path.strip_prefix(root)
        {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            out.push(serde_json::json!({
                "path": rel.to_string_lossy(),
                "size": size,
            }));
        }
    }
}

/// `peckboard_list_project_files` — list files (relative path + byte size)
/// under the caller's folder, for size-balanced scope partitioning. Paths are
/// relative to the folder root; `truncated` is `true` if the file cap was hit.
pub(crate) fn list_project_files_impl(db: &Db, inv: &InvocationContext) -> String {
    let root = match caller_folder_root(db, inv) {
        Ok(r) => r,
        Err(e) => return error_json(e),
    };
    let mut files = Vec::new();
    let mut truncated = false;
    walk_project_files(&root, &root, 0, &mut files, &mut truncated);
    serde_json::json!({ "files": files, "truncated": truncated }).to_string()
}

/// `peckboard_read_file` — read one UTF-8 text file under the caller's folder.
/// The path must be relative and stay within the folder; content is capped at
/// `PLUGIN_FS_MAX_READ_BYTES` (`truncated` flags a clipped read).
pub(crate) fn read_file_impl(db: &Db, input: &str, inv: &InvocationContext) -> String {
    let req: ReadFileRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    let root = match caller_folder_root(db, inv) {
        Ok(r) => r,
        Err(e) => return error_json(e),
    };
    let rel = Path::new(&req.path);
    // Reject anything but plain, descending relative segments *before* touching
    // the filesystem: no absolute/root/prefix, no `..`.
    if rel.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return error_json("path must be relative and within the project folder");
    }
    let target = root.join(rel);
    // Canonicalize and re-check containment — defeats a symlink that points
    // outside the folder even though every textual segment looked safe.
    let canon = match std::fs::canonicalize(&target) {
        Ok(p) => p,
        Err(e) => return error_json(format!("file not found: {e}")),
    };
    if !canon.starts_with(&root) {
        return error_json("path escapes the project folder");
    }
    let meta = match std::fs::metadata(&canon) {
        Ok(m) => m,
        Err(e) => return error_json(e),
    };
    if !meta.is_file() {
        return error_json("not a file");
    }
    let bytes = match std::fs::read(&canon) {
        Ok(b) => b,
        Err(e) => return error_json(e),
    };
    let truncated = bytes.len() > PLUGIN_FS_MAX_READ_BYTES;
    let slice = &bytes[..bytes.len().min(PLUGIN_FS_MAX_READ_BYTES)];
    // Lossy so a clip at a multi-byte boundary (or a stray non-UTF-8 byte in an
    // otherwise-text file) still returns usable content rather than erroring.
    let content = String::from_utf8_lossy(slice).into_owned();
    serde_json::json!({
        "content": content,
        "truncated": truncated,
        "size": meta.len(),
    })
    .to_string()
}

// ── Live agent dispatch (gated, scoped, fire-and-forget) ──────────────
//
// `dispatch_capture` / `resume_session` schedule an agent run on a session and
// return immediately — the heavy work runs on the async runtime, so the
// synchronous WASM call stays well under its timeout. They authorize the target
// by *visibility* (`fetch_visible_session`: in the caller's folder, project, or
// global), NOT ownership: delivery legitimately targets sessions the plugin
// does not own — above all the *asking* session when an expert replies — which
// is exactly the within-scope delivery core's own expert flow performs. The
// §7.4 boundary preserved here is "no cross-folder/project escalation". They
// refuse if the live host isn't bound (e.g. a headless/test manager).

#[derive(Deserialize)]
struct DispatchCaptureRequest {
    session_id: String,
    prompt: String,
}

#[derive(Deserialize)]
struct ResumeSessionRequest {
    session_id: String,
    text: String,
}

/// `peckboard_dispatch_capture` — kick off a fresh capture run on a session in
/// the caller's scope (e.g. an expert reading its slice).
pub(crate) fn dispatch_capture_impl(
    db: &Db,
    input: &str,
    inv: &InvocationContext,
    live: Option<Arc<dyn LiveHost>>,
) -> String {
    let req: DispatchCaptureRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    if let Err(e) = fetch_visible_session(db, req.session_id.trim(), inv) {
        return error_json(e);
    }
    let Some(live) = live else {
        return error_json("live dispatch unavailable");
    };
    live.dispatch_capture(req.session_id.trim().to_string(), req.prompt);
    serde_json::json!({ "ok": true }).to_string()
}

/// `peckboard_resume_session` — deliver a message to a session in the caller's
/// scope and resume it (hand an expert a question, or an answer back to the
/// asker).
pub(crate) fn resume_session_impl(
    db: &Db,
    input: &str,
    inv: &InvocationContext,
    live: Option<Arc<dyn LiveHost>>,
) -> String {
    let req: ResumeSessionRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    if let Err(e) = fetch_visible_session(db, req.session_id.trim(), inv) {
        return error_json(e);
    }
    let Some(live) = live else {
        return error_json("live dispatch unavailable");
    };
    live.resume_session(req.session_id.trim().to_string(), req.text);
    serde_json::json!({ "ok": true }).to_string()
}

/// Clone the `Db` and calling plugin id out of the host-function user data
/// without holding the mutex across the (potentially DB-locking) call. A
/// poisoned mutex is surfaced as an `Err` rather than a panic, keeping the
/// FFI boundary safe.
fn state_from(user_data: &UserData<HostState>) -> Result<(Db, String), Error> {
    let state = user_data.get()?;
    let state = state
        .lock()
        .map_err(|_| anyhow::anyhow!("plugin host state mutex poisoned"))?;
    Ok((state.db.clone(), state.plugin_id.clone()))
}

/// Like [`state_from`] but also returns whether the plugin holds `permission`.
/// Gated host functions call this and return an `{"error": ...}` to the plugin
/// when the permission is absent, rather than performing the action.
fn state_and_permission(
    user_data: &UserData<HostState>,
    permission: &str,
) -> Result<(Db, String, bool), Error> {
    let state = user_data.get()?;
    let state = state
        .lock()
        .map_err(|_| anyhow::anyhow!("plugin host state mutex poisoned"))?;
    let granted = state
        .permissions
        .read()
        .map_err(|_| anyhow::anyhow!("plugin permission set poisoned"))?
        .contains(permission);
    Ok((state.db.clone(), state.plugin_id.clone(), granted))
}

/// Like [`state_and_permission`] but also returns the **trusted** invocation
/// context. Scoped host functions (sessions, events, project files) call this:
/// they derive the caller's session/project/folder from the returned
/// [`InvocationContext`] — set host-side from the verified MCP token — never
/// from plugin-supplied arguments. Returns `None` for the context when the
/// plugin is not inside an `mcp.tool.invoke` dispatch (e.g. `init`), so callers
/// refuse with a clear error rather than acting unscoped.
fn state_permission_and_invocation(
    user_data: &UserData<HostState>,
    permission: &str,
) -> Result<(Db, String, bool, Option<InvocationContext>), Error> {
    let state = user_data.get()?;
    let state = state
        .lock()
        .map_err(|_| anyhow::anyhow!("plugin host state mutex poisoned"))?;
    let granted = state
        .permissions
        .read()
        .map_err(|_| anyhow::anyhow!("plugin permission set poisoned"))?
        .contains(permission);
    let invocation = effective_context(&state)?;
    Ok((
        state.db.clone(),
        state.plugin_id.clone(),
        granted,
        invocation,
    ))
}

/// The effective caller context a scoped host function should use: an in-flight
/// MCP invocation's verified scope if present, else — when the plugin is in an
/// authenticated user request — a full-authority context derived from the user.
/// `None` when the plugin is in neither (e.g. `init`, or a public request), so
/// scoped functions refuse.
fn effective_context(
    state: &std::sync::MutexGuard<'_, HostState>,
) -> Result<Option<InvocationContext>, Error> {
    if let Some(inv) = state
        .invocation
        .read()
        .map_err(|_| anyhow::anyhow!("plugin invocation context poisoned"))?
        .clone()
    {
        return Ok(Some(inv));
    }
    Ok(state
        .user
        .read()
        .map_err(|_| anyhow::anyhow!("plugin user context poisoned"))?
        .as_ref()
        .map(UserContext::as_invocation))
}

/// Like [`state_permission_and_invocation`] but also clones the late-bound
/// [`LiveHost`] (if any). The live host functions need it to schedule agent
/// dispatch after they've authorized the target session.
#[allow(clippy::type_complexity)]
fn state_permission_invocation_and_live(
    user_data: &UserData<HostState>,
    permission: &str,
) -> Result<
    (
        Db,
        String,
        bool,
        Option<InvocationContext>,
        Option<Arc<dyn LiveHost>>,
    ),
    Error,
> {
    let state = user_data.get()?;
    let state = state
        .lock()
        .map_err(|_| anyhow::anyhow!("plugin host state mutex poisoned"))?;
    let granted = state
        .permissions
        .read()
        .map_err(|_| anyhow::anyhow!("plugin permission set poisoned"))?
        .contains(permission);
    let invocation = effective_context(&state)?;
    let live = state
        .live
        .read()
        .map_err(|_| anyhow::anyhow!("plugin live host poisoned"))?
        .clone();
    Ok((
        state.db.clone(),
        state.plugin_id.clone(),
        granted,
        invocation,
        live,
    ))
}

host_fn!(peckboard_list_projects(user_data: HostState; _input: String) -> String {
    let (db, _plugin_id) = state_from(&user_data)?;
    Ok(list_projects_impl(&db))
});

host_fn!(peckboard_list_cards(user_data: HostState; input: String) -> String {
    let (db, _plugin_id) = state_from(&user_data)?;
    Ok(list_cards_impl(&db, &input))
});

host_fn!(peckboard_create_card(user_data: HostState; input: String) -> String {
    let (db, _plugin_id) = state_from(&user_data)?;
    Ok(create_card_impl(&db, &input))
});

host_fn!(peckboard_get_plugin_setting(user_data: HostState; input: String) -> String {
    let (db, plugin_id) = state_from(&user_data)?;
    Ok(get_plugin_setting_impl(&db, &plugin_id, &input))
});

host_fn!(peckboard_set_plugin_setting(user_data: HostState; input: String) -> String {
    let (db, plugin_id) = state_from(&user_data)?;
    Ok(set_plugin_setting_impl(&db, &plugin_id, &input))
});

host_fn!(peckboard_list_plugin_settings(user_data: HostState; _input: String) -> String {
    let (db, plugin_id) = state_from(&user_data)?;
    Ok(list_plugin_settings_impl(&db, &plugin_id))
});

// ── Generic plugin storage (gated) ────────────────────────────────────

host_fn!(peckboard_store_put(user_data: HostState; input: String) -> String {
    let (db, plugin_id, ok) = state_and_permission(&user_data, "data_store")?;
    if !ok { return Ok(error_json("plugin lacks the 'data_store' permission")); }
    Ok(store_put_impl(&db, &plugin_id, &input))
});

host_fn!(peckboard_store_get(user_data: HostState; input: String) -> String {
    let (db, plugin_id, ok) = state_and_permission(&user_data, "data_store")?;
    if !ok { return Ok(error_json("plugin lacks the 'data_store' permission")); }
    Ok(store_get_impl(&db, &plugin_id, &input))
});

host_fn!(peckboard_store_list(user_data: HostState; input: String) -> String {
    let (db, plugin_id, ok) = state_and_permission(&user_data, "data_store")?;
    if !ok { return Ok(error_json("plugin lacks the 'data_store' permission")); }
    Ok(store_list_impl(&db, &plugin_id, &input))
});

host_fn!(peckboard_store_delete(user_data: HostState; input: String) -> String {
    let (db, plugin_id, ok) = state_and_permission(&user_data, "data_store")?;
    if !ok { return Ok(error_json("plugin lacks the 'data_store' permission")); }
    Ok(store_delete_impl(&db, &plugin_id, &input))
});

host_fn!(peckboard_session_meta_set(user_data: HostState; input: String) -> String {
    let (db, plugin_id, ok) = state_and_permission(&user_data, "session_write")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_write' permission")); }
    Ok(session_meta_set_impl(&db, &plugin_id, &input))
});

host_fn!(peckboard_session_meta_get(user_data: HostState; input: String) -> String {
    let (db, plugin_id, ok) = state_and_permission(&user_data, "session_read")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_read' permission")); }
    Ok(session_meta_get_impl(&db, &plugin_id, &input))
});

// ── Generic session / event host functions (gated, scoped) ────────────
// Each refuses if called outside an `mcp.tool.invoke` (no trusted context).

host_fn!(peckboard_create_session(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok, inv) = state_permission_and_invocation(&user_data, "session_write")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_write' permission")); }
    let Some(inv) = inv else { return Ok(error_json("no caller context; peckboard_create_session is only callable during a tool invocation")); };
    Ok(create_session_impl(&db, &input, &inv))
});

host_fn!(peckboard_get_session(user_data: HostState; input: String) -> String {
    let (db, plugin_id, ok, inv) = state_permission_and_invocation(&user_data, "session_read")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_read' permission")); }
    let Some(inv) = inv else { return Ok(error_json("no caller context; peckboard_get_session is only callable during a tool invocation")); };
    Ok(get_session_impl(&db, &plugin_id, &input, &inv))
});

host_fn!(peckboard_list_sessions(user_data: HostState; input: String) -> String {
    let (db, plugin_id, ok, inv) = state_permission_and_invocation(&user_data, "session_read")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_read' permission")); }
    let Some(inv) = inv else { return Ok(error_json("no caller context; peckboard_list_sessions is only callable during a tool invocation")); };
    Ok(list_sessions_impl(&db, &plugin_id, &input, &inv))
});

host_fn!(peckboard_update_session(user_data: HostState; input: String) -> String {
    let (db, plugin_id, ok, inv) = state_permission_and_invocation(&user_data, "session_write")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_write' permission")); }
    let Some(inv) = inv else { return Ok(error_json("no caller context; peckboard_update_session is only callable during a tool invocation")); };
    Ok(update_session_impl(&db, &plugin_id, &input, &inv))
});

host_fn!(peckboard_append_event(user_data: HostState; input: String) -> String {
    let (db, plugin_id, ok, inv) = state_permission_and_invocation(&user_data, "event_append")?;
    if !ok { return Ok(error_json("plugin lacks the 'event_append' permission")); }
    let Some(inv) = inv else { return Ok(error_json("no caller context; peckboard_append_event is only callable during a tool invocation")); };
    Ok(append_event_impl(&db, &plugin_id, &input, &inv))
});

host_fn!(peckboard_list_project_files(user_data: HostState; _input: String) -> String {
    let (db, _plugin_id, ok, inv) = state_permission_and_invocation(&user_data, "project_files_read")?;
    if !ok { return Ok(error_json("plugin lacks the 'project_files_read' permission")); }
    let Some(inv) = inv else { return Ok(error_json("no caller context; peckboard_list_project_files is only callable during a tool invocation")); };
    Ok(list_project_files_impl(&db, &inv))
});

host_fn!(peckboard_read_file(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok, inv) = state_permission_and_invocation(&user_data, "project_files_read")?;
    if !ok { return Ok(error_json("plugin lacks the 'project_files_read' permission")); }
    let Some(inv) = inv else { return Ok(error_json("no caller context; peckboard_read_file is only callable during a tool invocation")); };
    Ok(read_file_impl(&db, &input, &inv))
});

host_fn!(peckboard_dispatch_capture(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok, inv, live) = state_permission_invocation_and_live(&user_data, "session_dispatch")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_dispatch' permission")); }
    let Some(inv) = inv else { return Ok(error_json("no caller context; peckboard_dispatch_capture is only callable during a tool invocation")); };
    Ok(dispatch_capture_impl(&db, &input, &inv, live))
});

host_fn!(peckboard_resume_session(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok, inv, live) = state_permission_invocation_and_live(&user_data, "session_dispatch")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_dispatch' permission")); }
    let Some(inv) = inv else { return Ok(error_json("no caller context; peckboard_resume_session is only callable during a tool invocation")); };
    Ok(resume_session_impl(&db, &input, &inv, live))
});

/// Build the host-function set a single loaded plugin is wired with. Every
/// function shares one `UserData<HostState>` (a cheap `Arc` clone of the live
/// `Db` plus this plugin's id). `plugin_id` namespaces the plugin-settings
/// functions to the caller's own rows — pass the loading plugin's id (its
/// `.wasm` file stem, the same id its `plugin_settings` rows are keyed by).
#[allow(clippy::too_many_arguments)]
pub(crate) fn host_functions(
    db: &Db,
    plugin_id: &str,
    permissions: Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    invocation: Arc<std::sync::RwLock<Option<InvocationContext>>>,
    live: Arc<std::sync::RwLock<Option<Arc<dyn LiveHost>>>>,
    user: Arc<std::sync::RwLock<Option<UserContext>>>,
) -> Vec<Function> {
    let ud = UserData::new(HostState {
        db: db.clone(),
        plugin_id: plugin_id.to_string(),
        permissions,
        invocation,
        live,
        user,
    });
    vec![
        Function::new(
            "peckboard_list_projects",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_list_projects,
        ),
        Function::new(
            "peckboard_list_cards",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_list_cards,
        ),
        Function::new(
            "peckboard_create_card",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_create_card,
        ),
        Function::new(
            "peckboard_get_plugin_setting",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_get_plugin_setting,
        ),
        Function::new(
            "peckboard_set_plugin_setting",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_set_plugin_setting,
        ),
        Function::new(
            "peckboard_list_plugin_settings",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_list_plugin_settings,
        ),
        Function::new(
            "peckboard_store_put",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_store_put,
        ),
        Function::new(
            "peckboard_store_get",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_store_get,
        ),
        Function::new(
            "peckboard_store_list",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_store_list,
        ),
        Function::new(
            "peckboard_store_delete",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_store_delete,
        ),
        Function::new(
            "peckboard_session_meta_set",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_session_meta_set,
        ),
        Function::new(
            "peckboard_session_meta_get",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_session_meta_get,
        ),
        Function::new(
            "peckboard_create_session",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_create_session,
        ),
        Function::new(
            "peckboard_get_session",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_get_session,
        ),
        Function::new(
            "peckboard_list_sessions",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_list_sessions,
        ),
        Function::new(
            "peckboard_update_session",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_update_session,
        ),
        Function::new(
            "peckboard_append_event",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_append_event,
        ),
        Function::new(
            "peckboard_list_project_files",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_list_project_files,
        ),
        Function::new(
            "peckboard_read_file",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_read_file,
        ),
        Function::new(
            "peckboard_dispatch_capture",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_dispatch_capture,
        ),
        Function::new(
            "peckboard_resume_session",
            [PTR],
            [PTR],
            ud,
            peckboard_resume_session,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{NewFolder, NewProject};

    #[test]
    fn store_impls_roundtrip_and_validate() {
        let db = Db::in_memory().unwrap();
        let pid = "experts";
        // put → get → list → delete via the host-fn impls (JSON in/out).
        let out = store_put_impl(
            &db,
            pid,
            r#"{"collection":"decisions","key":"d1","data":{"q":"why"}}"#,
        );
        assert!(out.contains("\"ok\":true"), "put: {out}");
        let got = store_get_impl(&db, pid, r#"{"collection":"decisions","key":"d1"}"#);
        assert!(got.contains("\"why\""), "get: {got}");
        let list = store_list_impl(&db, pid, r#"{"collection":"decisions"}"#);
        assert!(list.contains("\"d1\""), "list: {list}");
        let del = store_delete_impl(&db, pid, r#"{"collection":"decisions","key":"d1"}"#);
        assert!(del.contains("\"deleted\":true"), "delete: {del}");
        // Missing/oversized identifiers are rejected, not stored.
        let bad = store_put_impl(&db, pid, r#"{"collection":"","key":"k","data":1}"#);
        assert!(
            bad.contains("error"),
            "empty collection should error: {bad}"
        );
    }

    #[test]
    fn session_meta_impls_roundtrip() {
        let db = Db::in_memory().unwrap();
        let set = session_meta_set_impl(
            &db,
            "experts",
            r#"{"session_id":"s1","data":{"kind":"pm"}}"#,
        );
        assert!(set.contains("\"ok\":true"), "set: {set}");
        let get = session_meta_get_impl(&db, "experts", r#"{"session_id":"s1"}"#);
        assert!(get.contains("\"pm\""), "get: {get}");
        // A session the plugin never tagged reads back null.
        let none = session_meta_get_impl(&db, "experts", r#"{"session_id":"nope"}"#);
        assert!(none.contains("null"), "absent meta should be null: {none}");
    }

    fn inv(project: Option<&str>, folder: Option<&str>) -> InvocationContext {
        InvocationContext {
            project_id: project.map(str::to_string),
            folder_id: folder.map(str::to_string),
            authority: false,
        }
    }

    /// The full-authority context an authenticated user request resolves to.
    fn inv_user() -> InvocationContext {
        InvocationContext {
            project_id: None,
            folder_id: None,
            authority: true,
        }
    }

    /// Under user authority a plugin reaches its own sessions across EVERY
    /// project/folder — the boundary an MCP tool call is held to does not apply
    /// (matching core's authenticated `/api/*` routes). It still only sees
    /// sessions it manages (its own `session_meta`).
    #[tokio::test]
    async fn user_authority_sees_all_owned_sessions() {
        let db = setup().await; // f1 / p1
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f2".into(),
            name: "Other".into(),
            path: "/tmp/f2u".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_project(NewProject {
            id: "p2".into(),
            name: "Other".into(),
            context: String::new(),
            folder_id: "f2".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "task".into(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: false,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts,
        })
        .await
        .unwrap();
        let pid = "experts";
        // One expert in each project, both marked by the plugin.
        let mut ids = Vec::new();
        for (proj, fold) in [("p1", "f1"), ("p2", "f2")] {
            let sid = serde_json::from_str::<serde_json::Value>(&create_session_impl(
                &db,
                r#"{"name":"expert"}"#,
                &inv(Some(proj), Some(fold)),
            ))
            .unwrap()["session"]["id"]
                .as_str()
                .unwrap()
                .to_string();
            session_meta_set_impl(
                &db,
                pid,
                &format!(r#"{{"session_id":"{sid}","data":{{"kind":"pm"}}}}"#),
            );
            ids.push(sid);
        }

        // A p1-scoped MCP caller sees only the p1 expert.
        let scoped = list_sessions_impl(&db, pid, "{}", &inv(Some("p1"), Some("f1")));
        let sv: serde_json::Value = serde_json::from_str(&scoped).unwrap();
        assert_eq!(
            sv["sessions"].as_array().unwrap().len(),
            1,
            "scoped: {scoped}"
        );

        // An authenticated user sees BOTH (across projects).
        let all = list_sessions_impl(&db, pid, "{}", &inv_user());
        let av: serde_json::Value = serde_json::from_str(&all).unwrap();
        assert_eq!(
            av["sessions"].as_array().unwrap().len(),
            2,
            "authority: {all}"
        );

        // ...and may read the cross-project one a scoped caller cannot.
        let cross = &ids[1]; // the p2 expert
        let scoped_get = get_session_impl(
            &db,
            pid,
            &format!(r#"{{"session_id":"{cross}"}}"#),
            &inv(Some("p1"), Some("f1")),
        );
        assert!(
            scoped_get.contains("not found"),
            "scoped must refuse: {scoped_get}"
        );
        let user_get = get_session_impl(
            &db,
            pid,
            &format!(r#"{{"session_id":"{cross}"}}"#),
            &inv_user(),
        );
        assert!(
            user_get.contains("\"session\""),
            "authority must read: {user_get}"
        );
    }

    /// The load-bearing test: the session host functions create rows in the
    /// caller's scope and refuse to read/update/append outside it, even when
    /// the plugin owns (has marked) the target. See `fetch_owned_visible_session`.
    #[tokio::test]
    async fn session_host_fns_are_owned_and_scoped() {
        let db = setup().await; // folder f1 / project p1
        // A second folder + project the caller must never reach.
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f2".into(),
            name: "Other".into(),
            path: "/tmp/f2".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_project(NewProject {
            id: "p2".into(),
            name: "Other".into(),
            context: String::new(),
            folder_id: "f2".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "task".into(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: false,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts,
        })
        .await
        .unwrap();

        let pid = "experts";
        let caller = inv(Some("p1"), Some("f1"));

        // create_session lands in the *caller's* folder/project, ignoring any
        // ids the plugin might try to supply.
        let out = create_session_impl(&db, r#"{"name":"expert: auth"}"#, &caller);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("error").is_none(), "create: {out}");
        assert_eq!(v["session"]["folder_id"], "f1");
        assert_eq!(v["session"]["project_id"], "p1");
        assert_eq!(v["session"]["is_expert"], false); // generic; expert-ness is meta
        let sid = v["session"]["id"].as_str().unwrap().to_string();

        // Before the plugin marks it, it doesn't "own" it → not found.
        let pre = get_session_impl(&db, pid, &format!(r#"{{"session_id":"{sid}"}}"#), &caller);
        assert!(pre.contains("not found"), "unowned read: {pre}");

        // Mark it as this plugin's expert session.
        session_meta_set_impl(
            &db,
            pid,
            &format!(r#"{{"session_id":"{sid}","data":{{"kind":"knowledge"}}}}"#),
        );

        // Now get/list/update/append all work for the owner+caller.
        let got = get_session_impl(&db, pid, &format!(r#"{{"session_id":"{sid}"}}"#), &caller);
        assert!(got.contains("expert: auth"), "owned read: {got}");

        let list = list_sessions_impl(&db, pid, "{}", &caller);
        let lv: serde_json::Value = serde_json::from_str(&list).unwrap();
        assert_eq!(lv["sessions"].as_array().unwrap().len(), 1, "list: {list}");
        assert_eq!(lv["sessions"][0]["meta"]["kind"], "knowledge");

        let upd = update_session_impl(
            &db,
            pid,
            &format!(r#"{{"session_id":"{sid}","name":"expert: auth+ws"}}"#),
            &caller,
        );
        assert!(upd.contains("auth+ws"), "update: {upd}");

        let ev = append_event_impl(
            &db,
            pid,
            &format!(r#"{{"session_id":"{sid}","kind":"user","data":{{"text":"hi"}}}}"#),
            &caller,
        );
        assert!(ev.contains("\"ok\":true"), "append: {ev}");

        // Ownership: a *different* plugin can't reach this plugin's session.
        let other = get_session_impl(
            &db,
            "intruder",
            &format!(r#"{{"session_id":"{sid}"}}"#),
            &caller,
        );
        assert!(other.contains("not found"), "cross-plugin read: {other}");

        // Scope escalation: a session this plugin owns but in p2/f2 is invisible
        // to a p1/f1 caller — even with a valid id.
        let foreign =
            create_session_impl(&db, r#"{"name":"foreign"}"#, &inv(Some("p2"), Some("f2")));
        let fid = serde_json::from_str::<serde_json::Value>(&foreign).unwrap()["session"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        session_meta_set_impl(
            &db,
            pid,
            &format!(r#"{{"session_id":"{fid}","data":{{}}}}"#),
        );
        let leak = get_session_impl(&db, pid, &format!(r#"{{"session_id":"{fid}"}}"#), &caller);
        assert!(
            leak.contains("not found"),
            "cross-project read must be refused: {leak}"
        );
        // …and it must not appear in the p1 caller's listing.
        let list2 = list_sessions_impl(&db, pid, "{}", &caller);
        let lv2: serde_json::Value = serde_json::from_str(&list2).unwrap();
        assert_eq!(
            lv2["sessions"].as_array().unwrap().len(),
            1,
            "foreign leaked into list: {list2}"
        );
    }

    /// Project-file access stays inside the caller's folder: ignored dirs are
    /// skipped, and `..` / symlink escapes are refused.
    #[tokio::test]
    async fn project_files_are_listed_and_contained() {
        use std::fs;
        let db = Db::in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("node_modules")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() { /* hi */ }").unwrap();
        fs::write(root.join("README.md"), "# readme").unwrap();
        fs::write(root.join("sub/deep.txt"), "deep").unwrap();
        fs::write(root.join(".git/config"), "secret").unwrap();
        fs::write(root.join("node_modules/x.js"), "vendored").unwrap();

        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "fX".into(),
            name: "Repo".into(),
            path: root.to_string_lossy().to_string(),
            created_at: ts,
        })
        .await
        .unwrap();
        let caller = inv(Some("p1"), Some("fX"));

        // Listing includes source files, excludes ignored dirs.
        let out = list_project_files_impl(&db, &caller);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let paths: Vec<String> = v["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap().to_string())
            .collect();
        assert!(paths.iter().any(|p| p == "src/main.rs"), "paths: {paths:?}");
        assert!(paths.iter().any(|p| p == "README.md"), "paths: {paths:?}");
        assert!(
            paths.iter().any(|p| p == "sub/deep.txt"),
            "paths: {paths:?}"
        );
        assert!(
            !paths
                .iter()
                .any(|p| p.contains(".git") || p.contains("node_modules")),
            "ignored dirs leaked: {paths:?}"
        );

        // Read a file inside the folder.
        let r = read_file_impl(&db, r#"{"path":"src/main.rs"}"#, &caller);
        assert!(r.contains("fn main"), "read: {r}");

        // `..` traversal is refused before touching the fs.
        let esc = read_file_impl(&db, r#"{"path":"../../etc/passwd"}"#, &caller);
        assert!(esc.contains("within the project folder"), "escape: {esc}");

        // A missing file is a clean error, not a panic.
        let miss = read_file_impl(&db, r#"{"path":"nope.txt"}"#, &caller);
        assert!(miss.contains("not found"), "missing: {miss}");

        // A symlink pointing outside the folder is refused by the canonicalized
        // containment check, even though its textual path looks in-bounds.
        #[cfg(unix)]
        {
            let secret = dir.path().parent().unwrap().join("outside_secret.txt");
            fs::write(&secret, "TOP SECRET").unwrap();
            std::os::unix::fs::symlink(&secret, root.join("link.txt")).unwrap();
            let leak = read_file_impl(&db, r#"{"path":"link.txt"}"#, &caller);
            assert!(
                leak.contains("escapes the project folder"),
                "symlink escape must be refused: {leak}"
            );
            let _ = fs::remove_file(&secret);
        }
    }

    /// Records the live calls it receives so tests can assert dispatch only
    /// happens after authorization.
    #[derive(Default)]
    struct RecordingLive {
        calls: std::sync::Mutex<Vec<String>>,
    }
    impl LiveHost for RecordingLive {
        fn dispatch_capture(&self, session_id: String, _prompt: String) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("dispatch:{session_id}"));
        }
        fn resume_session(&self, session_id: String, _text: String) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("resume:{session_id}"));
        }
    }

    #[tokio::test]
    async fn live_dispatch_is_scoped_and_requires_binding() {
        let db = setup().await; // folder f1 / project p1
        let pid = "experts";
        let caller = inv(Some("p1"), Some("f1"));

        // An expert session the plugin owns.
        let sid = serde_json::from_str::<serde_json::Value>(&create_session_impl(
            &db,
            r#"{"name":"expert: auth"}"#,
            &caller,
        ))
        .unwrap()["session"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        session_meta_set_impl(
            &db,
            pid,
            &format!(r#"{{"session_id":"{sid}","data":{{"kind":"knowledge"}}}}"#),
        );

        let live = Arc::new(RecordingLive::default());
        let live_dyn: Arc<dyn LiveHost> = live.clone();

        // Authorized dispatch + resume to the owned (visible) expert reach the
        // live host.
        let d = dispatch_capture_impl(
            &db,
            &format!(r#"{{"session_id":"{sid}","prompt":"read your scope"}}"#),
            &caller,
            Some(live_dyn.clone()),
        );
        assert!(d.contains("\"ok\":true"), "dispatch: {d}");
        let r = resume_session_impl(
            &db,
            &format!(r#"{{"session_id":"{sid}","text":"question?"}}"#),
            &caller,
            Some(live_dyn.clone()),
        );
        assert!(r.contains("\"ok\":true"), "resume: {r}");

        // Delivery to a *visible but NOT owned* session is allowed — this is
        // the asker-reply case (an expert answering back to the session that
        // asked). Seed a plain session in the caller's scope and resume it.
        let asker = serde_json::from_str::<serde_json::Value>(&create_session_impl(
            &db,
            r#"{"name":"asker"}"#,
            &caller,
        ))
        .unwrap()["session"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        // (No session_meta_set → the plugin does not "own" it.)
        let reply = resume_session_impl(
            &db,
            &format!(r#"{{"session_id":"{asker}","text":"here's your answer"}}"#),
            &caller,
            Some(live_dyn.clone()),
        );
        assert!(reply.contains("\"ok\":true"), "reply to asker: {reply}");
        assert_eq!(
            *live.calls.lock().unwrap(),
            vec![
                format!("dispatch:{sid}"),
                format!("resume:{sid}"),
                format!("resume:{asker}")
            ]
        );

        // A session OUTSIDE the caller's folder/project is refused (the §7.4
        // boundary) and never dispatched. Put it in p2/f2 (seeded below).
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f2".into(),
            name: "Other".into(),
            path: "/tmp/f2b".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_project(NewProject {
            id: "p2".into(),
            name: "Other".into(),
            context: String::new(),
            folder_id: "f2".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "task".into(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: false,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts,
        })
        .await
        .unwrap();
        let foreign = serde_json::from_str::<serde_json::Value>(&create_session_impl(
            &db,
            r#"{"name":"foreign"}"#,
            &inv(Some("p2"), Some("f2")),
        ))
        .unwrap()["session"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let refused = dispatch_capture_impl(
            &db,
            &format!(r#"{{"session_id":"{foreign}","prompt":"x"}}"#),
            &caller,
            Some(live_dyn.clone()),
        );
        assert!(refused.contains("not found"), "cross-scope: {refused}");
        assert_eq!(
            live.calls.lock().unwrap().len(),
            3,
            "must not dispatch cross-scope"
        );

        // With no live host bound, an authorized call degrades cleanly.
        let unbound = dispatch_capture_impl(
            &db,
            &format!(r#"{{"session_id":"{sid}","prompt":"x"}}"#),
            &caller,
            None,
        );
        assert!(
            unbound.contains("live dispatch unavailable"),
            "unbound: {unbound}"
        );
    }

    async fn setup() -> Db {
        let db = Db::in_memory().unwrap();
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "Folder".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_project(NewProject {
            id: "p1".into(),
            name: "Project".into(),
            context: String::new(),
            folder_id: "f1".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "task".into(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: false,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts,
        })
        .await
        .unwrap();
        db
    }

    #[tokio::test]
    async fn create_then_list_card_roundtrip() {
        let db = setup().await;

        let out = create_card_impl(
            &db,
            &serde_json::json!({ "project_id": "p1", "title": "Hello", "priority": 1 }).to_string(),
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("error").is_none(), "unexpected error: {out}");
        assert_eq!(v["card"]["title"], "Hello");
        assert_eq!(v["card"]["project_id"], "p1");
        // Workflow inherited from the project; step defaults to backlog.
        assert_eq!(v["card"]["workflow"], "task");
        assert_eq!(v["card"]["step"], "backlog");

        // Project-scoped list finds it.
        let out = list_cards_impl(&db, &serde_json::json!({ "project_id": "p1" }).to_string());
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["cards"].as_array().unwrap().len(), 1);

        // Global list (no project filter) finds it too.
        let out = list_cards_impl(&db, "{}");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["cards"].as_array().unwrap().len(), 1);

        // Step filter that matches nothing returns an empty list.
        let out = list_cards_impl(&db, &serde_json::json!({ "step": "done" }).to_string());
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["cards"].as_array().unwrap().len(), 0);

        // Projects listing.
        let out = list_projects_impl(&db);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["projects"].as_array().unwrap().len(), 1);
        assert_eq!(v["projects"][0]["id"], "p1");
    }

    #[tokio::test]
    async fn create_card_unknown_project_is_error_not_panic() {
        let db = setup().await;
        let out = create_card_impl(
            &db,
            &serde_json::json!({ "project_id": "nope", "title": "x" }).to_string(),
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error"], "project not found");
    }

    #[tokio::test]
    async fn invalid_priority_and_workflow_are_errors() {
        let db = setup().await;

        let out = create_card_impl(
            &db,
            &serde_json::json!({ "project_id": "p1", "title": "x", "priority": 99 }).to_string(),
        );
        assert!(out.contains("invalid priority"), "got: {out}");

        let out = create_card_impl(
            &db,
            &serde_json::json!({ "project_id": "p1", "title": "x", "workflow": "nope" })
                .to_string(),
        );
        assert!(out.contains("unknown workflow id"), "got: {out}");
    }

    #[tokio::test]
    async fn malformed_json_is_error_not_panic() {
        let db = setup().await;
        assert!(create_card_impl(&db, "not json").contains("invalid request"));
        assert!(list_cards_impl(&db, "not json").contains("invalid request"));
    }

    #[tokio::test]
    async fn plugin_setting_set_get_roundtrip_and_is_plugin_scoped() {
        let db = Db::in_memory().unwrap();

        // Set a value for plugin "api".
        let out = set_plugin_setting_impl(
            &db,
            "api",
            &serde_json::json!({ "key": "keys", "value": [{ "key": "k1", "scope": "read" }] })
                .to_string(),
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], true, "unexpected: {out}");

        // Get it back verbatim (no redaction — the owner needs the real value).
        let out = get_plugin_setting_impl(
            &db,
            "api",
            &serde_json::json!({ "key": "keys" }).to_string(),
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["value"][0]["key"], "k1");
        assert_eq!(v["value"][0]["scope"], "read");

        // A DIFFERENT plugin sees nothing under the same key — namespaced.
        let out = get_plugin_setting_impl(
            &db,
            "other",
            &serde_json::json!({ "key": "keys" }).to_string(),
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["value"].is_null(), "cross-plugin read leaked: {out}");

        // list is scoped too: "api" has one key, "other" has none.
        let out = list_plugin_settings_impl(&db, "api");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["settings"].as_object().unwrap().len(), 1);
        let out = list_plugin_settings_impl(&db, "other");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["settings"].as_object().unwrap().len(), 0);

        // A null value deletes the key, so a later get is null again.
        let out = set_plugin_setting_impl(
            &db,
            "api",
            &serde_json::json!({ "key": "keys", "value": null }).to_string(),
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&out).unwrap()["ok"],
            true
        );
        let out = get_plugin_setting_impl(
            &db,
            "api",
            &serde_json::json!({ "key": "keys" }).to_string(),
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["value"].is_null());
    }

    #[tokio::test]
    async fn plugin_setting_validates_inputs_without_panic() {
        let db = Db::in_memory().unwrap();

        // Malformed JSON → error, not panic.
        assert!(get_plugin_setting_impl(&db, "api", "not json").contains("invalid request"));
        assert!(set_plugin_setting_impl(&db, "api", "not json").contains("invalid request"));

        // Empty key is rejected on both read and write.
        assert!(
            get_plugin_setting_impl(&db, "api", &serde_json::json!({ "key": "  " }).to_string())
                .contains("key is required")
        );
        assert!(
            set_plugin_setting_impl(
                &db,
                "api",
                &serde_json::json!({ "key": "", "value": 1 }).to_string()
            )
            .contains("key is required")
        );

        // Oversized key and value are rejected.
        let big_key = "k".repeat(MAX_SETTING_KEY_LEN + 1);
        assert!(
            set_plugin_setting_impl(
                &db,
                "api",
                &serde_json::json!({ "key": big_key, "value": 1 }).to_string()
            )
            .contains("key too long")
        );
        let big_value = "v".repeat(MAX_SETTING_VALUE_LEN + 1);
        assert!(
            set_plugin_setting_impl(
                &db,
                "api",
                &serde_json::json!({ "key": "k", "value": big_value }).to_string()
            )
            .contains("value too large")
        );
    }
}
