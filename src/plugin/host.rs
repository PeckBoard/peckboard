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

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

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
    /// App data dir — where `service::browser_runs` records test runs; the
    /// browser-run host functions read from it (gated by `browser_runs_read`).
    data_dir: std::path::PathBuf,
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
/// An attachment delivered with [`LiveHost::send_message`] — decoded image or
/// file bytes the receiving agent gets on the user message. The plugin passes
/// these base64-encoded; the host function decodes them before constructing one.
#[derive(Clone, Debug)]
pub struct LiveAttachment {
    pub filename: String,
    pub mime_type: String,
    pub data: Vec<u8>,
}

pub trait LiveHost: Send + Sync {
    /// Force a fresh capture run on `session_id` with `prompt` (maps to
    /// `ExpertDispatcher::dispatch_capture`).
    fn dispatch_capture(&self, session_id: String, prompt: String);
    /// Deliver `text` to `session_id` and resume it — spawn if idle, queue /
    /// inject if running (maps to `ExpertDispatcher::resume_session`).
    fn resume_session(&self, session_id: String, text: String);
    /// Deliver `text` plus `attachments` (images/files) to `session_id` and
    /// resume it, like [`Self::resume_session`] but with attachments on the
    /// user message. No-op default for impls that don't support attachments.
    fn send_message(&self, _session_id: String, _text: String, _attachments: Vec<LiveAttachment>) {}
    /// Persist a `user` event carrying `data` on `session_id`, broadcast it
    /// (same frame the session routes emit), then deliver `text` to the
    /// agent and resume — spawn if idle, queue/inject if running. The
    /// transcript-writing twin of [`Self::resume_session`]: the caller keeps
    /// the persisted `data` and the delivered `text` consistent (e.g. the
    /// pre-hatcher stores `{text, pre_hatch: {original}}` and delivers
    /// `text`). No-op default.
    fn deliver_user_message(&self, _session_id: String, _text: String, _data: serde_json::Value) {}
    /// Interrupt the in-flight turn on `session_id` (cancel the current run
    /// without deleting the session). Fire-and-forget; no-op default.
    fn interrupt_session(&self, _session_id: String) {}
    /// Terminate the long-lived agent process for `session_id` (kill it
    /// between turns; the next message starts fresh). No-op default.
    fn terminate_agent(&self, _session_id: String) {}
    /// Clear `session_id`: cancel any run, wipe its events / todos /
    /// attachments, and reset its conversation. Fire-and-forget; no-op default.
    fn clear_session(&self, _session_id: String) {}
    /// Gracefully recycle the agent process for `session_id`: wind the child
    /// down after its current turn (immediately when idle) so the next
    /// message spawns with the session's current config. Used after a
    /// plugin-driven model/effort change — a live child keeps its spawn-time
    /// model and account credentials, so reusing it would keep answering (and
    /// billing) as the old model/account. Fire-and-forget; no-op default.
    fn recycle_agent_after_turn(&self, _session_id: String) {}
    /// Emit a single-question user prompt to `session_id` (same UI surface as
    /// the worker `ask_user` MCP tool: a "question" event + broadcast). `token`
    /// is an opaque correlation id stored on the question so the plugin can
    /// later resolve the answer (see `get_answer_impl`). When
    /// `redirect_session_id` is set, the user's answer resumes THAT session
    /// instead of the asker — the pre-hatcher's clarifying flow, where the
    /// question renders on the chat session but the answer must feed the temp
    /// research session. Fire-and-forget; a no-op in headless/test contexts.
    /// The caller has already authorized the target session(s).
    fn ask_user(
        &self,
        _session_id: String,
        _question: String,
        _options: Vec<String>,
        _token: String,
        _redirect_session_id: Option<String>,
    ) {
    }
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
    /// The caller's own session id (set host-side from the verified MCP token,
    /// never plugin-supplied), so a scoped host function can act on the calling
    /// session — e.g. emit a question to it — without trusting a plugin
    /// argument. `None` outside an MCP invocation (e.g. an authed UI request).
    #[serde(rename = "sessionId", default)]
    pub session_id: Option<String>,
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
    /// Optional folder scope for this request, resolved by
    /// [`crate::plugin::manager::PluginManager::serve_http_authed`] from a
    /// caller-supplied project/session id (verified to exist). When set, the
    /// plugin's folder-scoped host functions (`read_file`, `exec`, …) run in
    /// this folder; `None` keeps the prior behaviour (no folder floor — global
    /// app-data calls only).
    pub folder_id: Option<String>,
    /// The project this request is scoped to, if it came from a project page.
    pub project_id: Option<String>,
    /// The session this request is scoped to, if it came from a session page.
    pub session_id: Option<String>,
}

impl UserContext {
    /// The caller context a host function sees for an authenticated user
    /// request: full authority, plus any project/session/folder scope the host
    /// resolved from the request (so folder-scoped reads land in that folder).
    fn as_invocation(&self) -> InvocationContext {
        InvocationContext {
            session_id: self.session_id.clone(),
            project_id: self.project_id.clone(),
            folder_id: self.folder_id.clone(),
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

/// JSON request for `peckboard_update_card`.
#[derive(Deserialize)]
struct UpdateCardRequest {
    card_id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    step: Option<String>,
    #[serde(default)]
    priority: Option<i32>,
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

    // Same canonical-effort validation as the MCP create_card handler —
    // junk like "very high" used to be stored verbatim.
    if let Some(e) = req.effort.as_deref()
        && !crate::provider::registry::standard_effort_levels()
            .iter()
            .any(|l| l.id == e)
    {
        return error_json(format!(
            "invalid effort `{e}` — use one of low|medium|high|xhigh|max (or omit it)"
        ));
    }
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
        system_prompt_name: None,
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

/// `peckboard_update_card` — update fields on an existing card (write).
///
/// Performs the same validation as the HTTP update route for priority and
/// effort. Does NOT fire hooks or broadcast — that is the calling plugin's
/// responsibility. Gated by the **`cards_write`** permission.
pub(crate) fn update_card_impl(db: &Db, input: &str) -> String {
    let req: UpdateCardRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };

    let card_id = req.card_id.trim();
    if card_id.is_empty() {
        return error_json("card_id is required");
    }

    if let Some(p) = req.priority {
        if !crate::routes::misc::is_valid_priority(p) {
            return error_json(format!(
                "invalid priority: {p} (allowed: 0=Critical, 1=High, 2=Medium, 3=Low)"
            ));
        }
    }

    if let Some(e) = req.effort.as_deref() {
        if !crate::provider::registry::standard_effort_levels()
            .iter()
            .any(|l| l.id == e)
        {
            return error_json(format!(
                "invalid effort `{e}` — use one of low|medium|high|xhigh|max (or omit it)"
            ));
        }
    }

    let now = chrono::Utc::now().to_rfc3339();

    // A non-empty block_reason implicitly blocks the card (matching the route).
    let block_reason = req
        .block_reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let blocked = req.blocked.or_else(|| block_reason.as_ref().map(|_| true));

    let update = crate::db::models::UpdateCard {
        title: req
            .title
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        description: req.description,
        step: req
            .step
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string()),
        priority: req.priority,
        workflow: None,
        model: req.model.map(Some),
        effort: req.effort.map(Some),
        worker_session_id: None,
        last_worker_session_id: None,
        handoff_context: None,
        blocked,
        block_reason: block_reason.map(Some),
        updated_at: Some(now),
        completed_at: None,
        system_prompt_name: None,
        model_autoswitch: None,
    };

    match db.update_card_blocking(card_id, update) {
        Ok(Some(card)) => serde_json::json!({ "card": card }).to_string(),
        Ok(None) => error_json("card not found"),
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
    /// Mark the session as an expert session. This sets the core
    /// `is_expert`/`expert_kind` columns so usage attribution and session
    /// listings classify it correctly; expert *knowledge* state still lives
    /// in the plugin's own `plugin_session_meta`.
    #[serde(default)]
    is_expert: bool,
    #[serde(default)]
    expert_kind: Option<String>,
    /// Optional system-prompt body to attach to the new session (appended
    /// after the standing Peckboard prompt, like `set_session_system_prompt`).
    /// The pre-hatcher uses this to run its research session under a
    /// configurable named prompt (default "fable 5").
    #[serde(default)]
    system_prompt: Option<String>,
    /// The library name the `system_prompt` body was resolved from, recorded
    /// on the session for display/audit. Optional and independent of the body.
    #[serde(default)]
    system_prompt_name: Option<String>,
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
/// folder and project. Expert *knowledge* state is the plugin's own metadata
/// (`peckboard_session_meta_set`); the optional `is_expert`/`expert_kind`
/// flags only classify the session for usage attribution and listings.
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
    // Cap the optional system-prompt body so a runaway prompt can't bloat a
    // session row (mirrors set_session_system_prompt's MAX_LEN).
    if let Some(ref sp) = req.system_prompt
        && sp.len() > 100_000
    {
        return error_json(format!(
            "system_prompt too long ({} > 100000 chars)",
            sp.len()
        ));
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
        is_expert: req.is_expert,
        expert_kind: req.expert_kind,
        knowledge_summary: None,
        knowledge_area: None,
        scope_path: None,
        is_permanent: false,
        repeating_task_id: None,
        system_prompt: req.system_prompt,
        handover_to_model: None,
        pending_handover_doc: None,
        worker_step: None,
        // Inherit the caller session's owner (experts/plugin-spawned sessions);
        // falls back to the sole user, else NULL on multi-user installs.
        user_id: db.resolve_spawned_session_owner_blocking(inv.session_id.as_deref()),
        context_reset_ts: None,
        model_autoswitch: None,
        system_prompt_name: req.system_prompt_name,
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
    live: Option<Arc<dyn LiveHost>>,
) -> String {
    let req: UpdateSessionRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    // Authorize against the *current* row before writing; keep the row as
    // the pre-patch snapshot for the model/effort change detection below.
    let prior = match fetch_owned_visible_session(db, plugin_id, req.session_id.trim(), inv) {
        Ok(s) => s,
        Err(e) => return error_json(e),
    };
    // A model/effort change must recycle any live child process — it was
    // spawned with the old `--model` and the old account's credential env,
    // so reusing it would keep answering (and billing) as the old
    // model/account. This path has no handover machinery (every change is a
    // direct write), so recycle on any actual change; mirrors the plain-
    // switch handling in the `PATCH /api/sessions/:id` route.
    let model_changed = matches!(&req.model, Some(m) if *m != prior.model);
    let effort_changed = matches!(&req.effort, Some(e) if *e != prior.effort);
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
        system_prompt: None,
        handover_to_model: None,
        pending_handover_doc: None,
        worker_step: None,
        context_reset_ts: None,
        model_autoswitch: None,
        pending_plan_review: None,
        system_prompt_name: None,
    };
    match db.update_session_blocking(req.session_id.trim(), update) {
        Ok(Some(session)) => {
            if (model_changed || effort_changed)
                && let Some(live) = live
            {
                live.recycle_agent_after_turn(req.session_id.trim().to_string());
            }
            serde_json::json!({ "session": session }).to_string()
        }
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
///
/// `pub(crate)` so the worker pipeline's codebase-map scan
/// ([`crate::worker::pipeline::scan_project_files`]) skips the exact same
/// dirs a plugin sees — a worker's map and an expert's view stay in sync.
pub(crate) fn is_ignored_fs_dir(name: &str) -> bool {
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

/// `peckboard_read_file_base64` — read one file under the caller's folder and
/// return its **raw bytes** base64-encoded, so binary content (images, etc.)
/// survives intact rather than being mangled by the lossy UTF-8 decode
/// `peckboard_read_file` applies. Same containment rules as `read_file`
/// (relative, in-folder, symlink-escape-checked) and the same
/// `PLUGIN_FS_MAX_READ_BYTES` cap (`truncated` flags a clipped read).
pub(crate) fn read_file_base64_impl(db: &Db, input: &str, inv: &InvocationContext) -> String {
    use base64::Engine as _;
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
    let base64 = base64::engine::general_purpose::STANDARD.encode(slice);
    serde_json::json!({
        "base64": base64,
        "truncated": truncated,
        "size": meta.len(),
    })
    .to_string()
}

/// Max bytes a single `peckboard_write_file` may write.
const PLUGIN_FS_MAX_WRITE_BYTES: usize = 8 * 1024 * 1024; // 8 MiB

#[derive(Deserialize)]
struct WriteFileRequest {
    path: String,
    content: String,
    #[serde(default)]
    append: bool,
    #[serde(default)]
    create_dirs: bool,
}

/// `peckboard_write_file` — write (or append to) one UTF-8 text file under the
/// caller's folder. The path must be relative and stay within the folder; the
/// **parent directory** is canonicalized and re-checked for containment so a
/// symlinked intermediate can't redirect the write outside the folder. With
/// `create_dirs`, missing in-folder parent directories are created first.
pub(crate) fn write_file_impl(db: &Db, input: &str, inv: &InvocationContext) -> String {
    let req: WriteFileRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    if req.content.len() > PLUGIN_FS_MAX_WRITE_BYTES {
        return error_json(format!(
            "content exceeds the {PLUGIN_FS_MAX_WRITE_BYTES}-byte write limit"
        ));
    }
    let root = match caller_folder_root(db, inv) {
        Ok(r) => r,
        Err(e) => return error_json(e),
    };
    let rel = Path::new(&req.path);
    // Reject anything but plain, descending relative segments before touching
    // the filesystem: no absolute/root/prefix, no `..`.
    if rel.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return error_json("path must be relative and within the project folder");
    }
    if rel.file_name().is_none() {
        return error_json("path must name a file");
    }
    let target = root.join(rel);
    let parent = match target.parent() {
        Some(p) => p.to_path_buf(),
        None => return error_json("path has no parent directory"),
    };
    // Materialize the parent dir (only when asked) so we can canonicalize it.
    if !parent.exists() {
        if req.create_dirs {
            if let Err(e) = std::fs::create_dir_all(&parent) {
                return error_json(format!("could not create parent directories: {e}"));
            }
        } else {
            return error_json("parent directory does not exist (pass create_dirs to make it)");
        }
    }
    // Canonicalize the parent and re-check containment — defeats a symlinked
    // intermediate directory that points outside the folder.
    let canon_parent = match std::fs::canonicalize(&parent) {
        Ok(p) => p,
        Err(e) => return error_json(format!("parent path unavailable: {e}")),
    };
    if !canon_parent.starts_with(&root) {
        return error_json("path escapes the project folder");
    }
    let final_path = canon_parent.join(rel.file_name().unwrap());
    // Refuse to clobber a non-file (e.g. a directory) at the target.
    if let Ok(meta) = std::fs::symlink_metadata(&final_path)
        && !meta.is_file()
    {
        return error_json("target exists and is not a regular file");
    }

    use std::io::Write as _;
    let open = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(req.append)
        .truncate(!req.append)
        .open(&final_path);
    let mut file = match open {
        Ok(f) => f,
        Err(e) => return error_json(format!("could not open file for writing: {e}")),
    };
    if let Err(e) = file.write_all(req.content.as_bytes()) {
        return error_json(format!("write failed: {e}"));
    }

    serde_json::json!({
        "ok": true,
        "path": req.path,
        "bytes_written": req.content.len(),
        "appended": req.append,
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

#[derive(Deserialize)]
struct DeliverMessageRequest {
    session_id: String,
    text: String,
    /// Optional extra fields persisted on the `user` event (a `text` field is
    /// filled in from `text` when absent), e.g. the pre-hatcher's
    /// `pre_hatch: {original, enriched}` block.
    #[serde(default)]
    data: Option<serde_json::Value>,
}

/// `peckboard_deliver_message` — persist a `user` event on a session in the
/// caller's scope, broadcast it, and resume the session with `text`: the
/// transcript-writing twin of `peckboard_resume_session`. Used by the
/// pre-hatcher to land the final (possibly enriched) chat message so the UI
/// shows exactly what the agent received.
pub(crate) fn deliver_message_impl(
    db: &Db,
    input: &str,
    inv: &InvocationContext,
    live: Option<Arc<dyn LiveHost>>,
) -> String {
    let req: DeliverMessageRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    if req.text.trim().is_empty() {
        return error_json("text is required");
    }
    if let Err(e) = fetch_visible_session(db, req.session_id.trim(), inv) {
        return error_json(e);
    }
    let mut data = req.data.unwrap_or_else(|| serde_json::json!({}));
    if !data.is_object() {
        return error_json("data must be a JSON object");
    }
    if data.get("text").is_none() {
        data["text"] = serde_json::Value::String(req.text.clone());
    }
    let Some(live) = live else {
        return error_json("live dispatch unavailable");
    };
    live.deliver_user_message(req.session_id.trim().to_string(), req.text, data);
    // Delivery is definitionally the END of a pre-hatch: once the final
    // message lands on the chat session, the temp research session has no
    // legitimate work left — but in practice its turn kept going and acted
    // on the user's message content itself (edited files, ran releases).
    // Kill its agent process here so nothing can run past the hand-off.
    if let Some(caller_id) = inv.session_id.as_deref()
        && let Ok(caller) = fetch_visible_session(db, caller_id, inv)
        && caller.expert_kind.as_deref()
            == Some(crate::service::mcp_server::PRE_HATCHER_EXPERT_KIND)
    {
        live.terminate_agent(caller.id);
    }
    serde_json::json!({ "ok": true }).to_string()
}

// ── Session control (gated: `session_control`) ────────────────────────
//
// Full control of ANY session by id (no folder/project boundary — the
// operator grants this by approving the plugin). Every action is
// fire-and-forget via the LiveHost; the host fn only validates the request
// and that the target session exists.

/// Largest single attachment a session-control `send_message` accepts,
/// matching the HTTP attachment upload cap.
const SEND_ATTACHMENT_MAX_BYTES: usize = 10 * 1024 * 1024;

#[derive(serde::Deserialize)]
struct SessionControlRequest {
    session_id: String,
}

#[derive(serde::Deserialize)]
struct FindSessionsRequest {
    /// Optional case-insensitive substring filter over session id, name,
    /// conversation_id, model, and folder_id. Omit to list every session.
    #[serde(default)]
    query: Option<String>,
}

#[derive(serde::Deserialize)]
struct SendMessageRequest {
    session_id: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    attachments: Vec<SendAttachment>,
}

#[derive(serde::Deserialize)]
struct SendAttachment {
    filename: String,
    mime_type: String,
    data_base64: String,
}

/// Look up a session by id with NO visibility boundary (session-control is
/// deliberately cross-folder). Returns a uniform "not found" so a bad id is
/// a clean error rather than a panic.
fn require_session(db: &Db, session_id: &str) -> Result<crate::db::models::Session, String> {
    let id = session_id.trim();
    if id.is_empty() {
        return Err("session_id is required".to_string());
    }
    match db.get_session_blocking(id) {
        Ok(Some(s)) => Ok(s),
        Ok(None) => Err(format!("session not found: {id}")),
        Err(e) => Err(e.to_string()),
    }
}

/// Shared body for the no-argument control actions (interrupt / terminate /
/// clear): parse `{session_id}`, confirm it exists, then fire `action`.
/// `peckboard_list_all_sessions` — folder-blind session discovery for
/// session-control. Where `peckboard_list_sessions` is ownership- and
/// visibility-scoped, this returns EVERY session in the instance so a
/// controller can resolve a target anywhere (e.g. map a `conversation_id` to
/// its `session_id`). Gated on the same `session_control` permission as the
/// action host functions, with no invocation/folder boundary. An optional
/// `query` filters (case-insensitive substring) over id, name,
/// conversation_id, model, and folder_id. Sessions come newest-first.
pub(crate) fn list_all_sessions_impl(db: &Db, input: &str) -> String {
    let req: FindSessionsRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    let sessions = match db.list_sessions_blocking() {
        Ok(s) => s,
        Err(e) => return error_json(e.to_string()),
    };
    let needle = req
        .query
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase);
    let matches = |s: &crate::db::models::Session, q: &str| {
        let opt = |v: &Option<String>| v.as_deref().unwrap_or("").to_lowercase();
        s.id.to_lowercase().contains(q)
            || s.name.to_lowercase().contains(q)
            || opt(&s.conversation_id).contains(q)
            || opt(&s.model).contains(q)
            || s.folder_id.to_lowercase().contains(q)
    };
    let out: Vec<serde_json::Value> = sessions
        .into_iter()
        .filter(|s| match &needle {
            None => true,
            Some(q) => matches(s, q),
        })
        .map(|s| {
            serde_json::json!({
                "session_id": s.id,
                "name": s.name,
                "folder_id": s.folder_id,
                "project_id": s.project_id,
                "conversation_id": s.conversation_id,
                "model": s.model,
                "is_worker": s.is_worker,
                "is_expert": s.is_expert,
                "card_id": s.card_id,
                "last_activity": s.last_activity,
            })
        })
        .collect();
    serde_json::json!({ "sessions": out }).to_string()
}

fn control_session(
    db: &Db,
    input: &str,
    live: Option<Arc<dyn LiveHost>>,
    action_name: &str,
    action: impl FnOnce(Arc<dyn LiveHost>, String),
) -> String {
    let req: SessionControlRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    let sid = match require_session(db, &req.session_id) {
        Ok(s) => s.id,
        Err(e) => return error_json(e),
    };
    let Some(live) = live else {
        return error_json("live control unavailable");
    };
    action(live, sid.clone());
    serde_json::json!({ "ok": true, "session_id": sid, "action": action_name }).to_string()
}

pub(crate) fn interrupt_session_impl(
    db: &Db,
    input: &str,
    live: Option<Arc<dyn LiveHost>>,
) -> String {
    control_session(db, input, live, "interrupt", |live, sid| {
        live.interrupt_session(sid)
    })
}

pub(crate) fn terminate_agent_impl(
    db: &Db,
    input: &str,
    live: Option<Arc<dyn LiveHost>>,
) -> String {
    control_session(db, input, live, "terminate", |live, sid| {
        live.terminate_agent(sid)
    })
}

pub(crate) fn clear_session_impl(db: &Db, input: &str, live: Option<Arc<dyn LiveHost>>) -> String {
    control_session(db, input, live, "clear", |live, sid| {
        live.clear_session(sid)
    })
}

/// `peckboard_send_message` — deliver a message (with optional base64 image /
/// file attachments) to any session and resume it.
pub(crate) fn send_message_impl(db: &Db, input: &str, live: Option<Arc<dyn LiveHost>>) -> String {
    use base64::Engine as _;

    let req: SendMessageRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    let sid = match require_session(db, &req.session_id) {
        Ok(s) => s.id,
        Err(e) => return error_json(e),
    };
    if req.text.trim().is_empty() && req.attachments.is_empty() {
        return error_json("send_message requires non-empty text or at least one attachment");
    }

    let mut attachments = Vec::with_capacity(req.attachments.len());
    for a in req.attachments {
        let data = match base64::engine::general_purpose::STANDARD.decode(a.data_base64.as_bytes())
        {
            Ok(b) => b,
            Err(e) => return error_json(format!("invalid base64 for '{}': {e}", a.filename)),
        };
        if data.len() > SEND_ATTACHMENT_MAX_BYTES {
            return error_json(format!(
                "attachment '{}' is {} bytes (max {SEND_ATTACHMENT_MAX_BYTES})",
                a.filename,
                data.len()
            ));
        }
        attachments.push(LiveAttachment {
            filename: a.filename,
            mime_type: a.mime_type,
            data,
        });
    }

    let Some(live) = live else {
        return error_json("live control unavailable");
    };
    let count = attachments.len();
    live.send_message(sid.clone(), req.text, attachments);
    serde_json::json!({ "ok": true, "session_id": sid, "attachments": count }).to_string()
}

// ── Outbound HTTP fetch (gated, SSRF-contained) ───────────────────────
//
// `peckboard_http_fetch` lets a plugin tool pull a public web page. The host
// owns the security boundary the WASM sandbox cannot: only `http`/`https`,
// only `GET`/`HEAD`, the resolved IP is checked against private/loopback/
// link-local ranges and **pinned** so a later re-resolution (DNS rebinding)
// can't swing to an internal address, redirects are NOT followed (a 3xx is
// returned verbatim so the caller re-fetches the validated `Location`), and
// the body is size- and time-capped. The actual request runs on a fresh
// `std::thread` with its own current-thread runtime so it never nests inside
// the host's tokio worker.

const HTTP_FETCH_MAX_BYTES: usize = 5 * 1024 * 1024; // 5 MiB body cap
const HTTP_FETCH_TIMEOUT_SECS: u64 = 20;

#[derive(Deserialize)]
struct HttpFetchRequest {
    url: String,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    headers: Option<BTreeMap<String, String>>,
}

/// Whether `ip` is in a range a public-web fetch must never reach — loopback,
/// private (RFC 1918 / ULA), link-local, CGNAT, unspecified, or otherwise
/// non-globally-routable. IPv4-mapped IPv6 is unwrapped first so `::ffff:10.x`
/// is judged as the v4 address it really is.
fn is_blocked_fetch_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || o[0] == 0 // "this network" 0.0.0.0/8
                || (o[0] == 100 && (o[1] & 0xc0) == 64) // CGNAT 100.64.0.0/10
                || o[0] >= 240 // reserved / multicast 240.0.0.0/4+
        }
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_blocked_fetch_ip(&IpAddr::V4(mapped));
            }
            let seg0 = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || (seg0 & 0xfe00) == 0xfc00 // unique-local fc00::/7
                || (seg0 & 0xffc0) == 0xfe80 // link-local fe80::/10
                || v6.is_multicast()
        }
    }
}

/// What [`perform_outbound_http`] sends: a validated URL with its resolved,
/// pinned address, plus everything request-shaped the two callers
/// (`http_fetch_impl`, `http_request_impl`) are allowed to vary.
struct OutboundHttp {
    url: reqwest::Url,
    host: String,
    pinned: SocketAddr,
    method: reqwest::Method,
    headers: BTreeMap<String, String>,
    body: Option<String>,
    timeout_secs: u64,
    user_agent: &'static str,
}

/// Run one pinned, redirect-less HTTP exchange and shape the response as the
/// host-fn JSON (`{"status","headers","body","truncated","final_url"}`).
/// Policy (methods, IP ranges, timeouts) is the caller's job — this owns only
/// the mechanics: a dedicated `std::thread` with its own current-thread
/// runtime (never nested in the host's tokio worker) and the 5 MiB body cap.
fn perform_outbound_http(req: OutboundHttp) -> Result<serde_json::Value, String> {
    let handle = std::thread::spawn(move || -> Result<serde_json::Value, String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("runtime: {e}"))?;
        rt.block_on(async move {
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(req.timeout_secs))
                .user_agent(req.user_agent)
                .resolve(&req.host, req.pinned)
                .build()
                .map_err(|e| format!("client: {e}"))?;
            let mut rb = client.request(req.method, req.url);
            for (k, v) in &req.headers {
                rb = rb.header(k.as_str(), v.as_str());
            }
            if let Some(b) = req.body {
                rb = rb.body(b);
            }
            let mut resp = rb
                .send()
                .await
                .map_err(|e| format!("request failed: {e}"))?;
            let status = resp.status().as_u16();
            let final_url = resp.url().to_string();
            let mut headers = serde_json::Map::new();
            for (k, v) in resp.headers().iter() {
                if let Ok(s) = v.to_str() {
                    headers.insert(
                        k.as_str().to_string(),
                        serde_json::Value::String(s.to_string()),
                    );
                }
            }
            // Stream the body with a hard cap via `chunk()` (no extra deps).
            let mut body: Vec<u8> = Vec::new();
            let mut truncated = false;
            loop {
                match resp.chunk().await {
                    Ok(Some(chunk)) => {
                        let room = HTTP_FETCH_MAX_BYTES.saturating_sub(body.len());
                        if chunk.len() > room {
                            body.extend_from_slice(&chunk[..room]);
                            truncated = true;
                            break;
                        }
                        body.extend_from_slice(&chunk);
                    }
                    Ok(None) => break,
                    Err(e) => return Err(format!("body read failed: {e}")),
                }
            }
            let body_str = String::from_utf8_lossy(&body).into_owned();
            Ok(serde_json::json!({
                "status": status,
                "headers": serde_json::Value::Object(headers),
                "body": body_str,
                "truncated": truncated,
                "final_url": final_url,
            }))
        })
    });
    match handle.join() {
        Ok(v) => v,
        Err(_) => Err("http thread panicked".into()),
    }
}

/// Parse + validate the URL shared by both outbound host functions: http/https
/// only, a host present, resolved via [`ToSocketAddrs`] and pinned. With
/// `require_public` the candidates are filtered through
/// [`is_blocked_fetch_ip`] (the `http_fetch` policy); without it the first
/// resolved address is taken as-is (the `http_request` policy).
fn validate_outbound_url(
    raw: &str,
    require_public: bool,
) -> Result<(reqwest::Url, String, SocketAddr), String> {
    let url = reqwest::Url::parse(raw.trim()).map_err(|e| format!("invalid url: {e}"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("only http and https urls are permitted".into());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "url has no host".to_string())?
        .to_string();
    let port = url.port_or_known_default().unwrap_or(0);
    let mut addrs = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("dns resolution failed: {e}"))?;
    let pinned = if require_public {
        addrs
            .find(|a| !is_blocked_fetch_ip(&a.ip()))
            .ok_or_else(|| {
                "host does not resolve to a public address (private/loopback blocked)".to_string()
            })?
    } else {
        addrs
            .next()
            .ok_or_else(|| "host resolved to no addresses".to_string())?
    };
    Ok((url, host, pinned))
}

/// `peckboard_http_fetch` — fetch a public-web URL on the plugin's behalf.
/// Input: `{"url", "method"?: "GET"|"HEAD", "headers"?: {..}}`. Output:
/// `{"status", "headers": {..}, "body", "truncated", "final_url"}` or an
/// `{"error"}` envelope. SSRF-contained: private/loopback targets, non-http
/// schemes, and non-GET/HEAD methods are refused.
pub(crate) fn http_fetch_impl(input: &str) -> String {
    let req: HttpFetchRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };

    let method = req.method.as_deref().unwrap_or("GET").to_ascii_uppercase();
    if method != "GET" && method != "HEAD" {
        return error_json("only GET and HEAD are permitted");
    }
    let method = if method == "HEAD" {
        reqwest::Method::HEAD
    } else {
        reqwest::Method::GET
    };

    let (url, host, pinned) = match validate_outbound_url(&req.url, true) {
        Ok(v) => v,
        Err(e) => return error_json(e),
    };

    match perform_outbound_http(OutboundHttp {
        url,
        host,
        pinned,
        method,
        headers: req.headers.unwrap_or_default(),
        body: None,
        timeout_secs: HTTP_FETCH_TIMEOUT_SECS,
        user_agent: "Peckboard-common-tools/0.1",
    }) {
        Ok(v) => v.to_string(),
        Err(e) => error_json(e),
    }
}

// ── Outbound HTTP request (gated, full-method, LAN-capable) ───────────
//
// `peckboard_http_request` is `http_fetch`'s wider sibling for plugins that
// integrate self-hosted services (an nginx-proxy-manager MCP endpoint on the
// LAN, a homelab API): every standard method, a request body, and — the whole
// point — private/loopback targets are allowed. That is server-side request
// forgery by design, so it sits behind its own `http_request` permission the
// operator must approve at install instead of silently widening `http_fetch`.
// The rest of the fetch containment stays: http/https schemes only, the
// resolved address is pinned for the exchange, redirects are returned
// verbatim, and the body shares the 5 MiB cap.

const HTTP_REQUEST_DEFAULT_TIMEOUT_SECS: u64 = 30;
const HTTP_REQUEST_MAX_TIMEOUT_SECS: u64 = 120;

#[derive(Deserialize)]
struct HttpRequestRequest {
    url: String,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// `peckboard_http_request` — perform an HTTP request on the plugin's behalf,
/// private/loopback targets included. Input: `{"url", "method"?: "GET"|"HEAD"
/// |"POST"|"PUT"|"PATCH"|"DELETE", "headers"?: {..}, "body"?,
/// "timeout_secs"?: 1..=120 (default 30)}`. Output: `{"status", "headers":
/// {..}, "body", "truncated", "final_url"}` or an `{"error"}` envelope.
pub(crate) fn http_request_impl(input: &str) -> String {
    let req: HttpRequestRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };

    let method = req.method.as_deref().unwrap_or("GET").to_ascii_uppercase();
    let method = match method.as_str() {
        "GET" => reqwest::Method::GET,
        "HEAD" => reqwest::Method::HEAD,
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "PATCH" => reqwest::Method::PATCH,
        "DELETE" => reqwest::Method::DELETE,
        other => return error_json(format!("method '{other}' is not permitted")),
    };

    let (url, host, pinned) = match validate_outbound_url(&req.url, false) {
        Ok(v) => v,
        Err(e) => return error_json(e),
    };

    let timeout_secs = req
        .timeout_secs
        .unwrap_or(HTTP_REQUEST_DEFAULT_TIMEOUT_SECS)
        .clamp(1, HTTP_REQUEST_MAX_TIMEOUT_SECS);

    match perform_outbound_http(OutboundHttp {
        url,
        host,
        pinned,
        method,
        headers: req.headers.unwrap_or_default(),
        body: req.body,
        timeout_secs,
        user_agent: "Peckboard-plugin/0.1",
    }) {
        Ok(v) => v.to_string(),
        Err(e) => error_json(e),
    }
}

// ── Allowlisted process execution (gated, scoped to the caller's folder) ──
//
// `peckboard_exec` runs a build/VCS/test command for a plugin tool (git, the
// project's test runner, …). The boundaries the WASM sandbox can't enforce
// live here: the executable must be a bare name on a fixed allowlist (no path,
// no shell — args are passed as an argv array, never interpolated), the cwd is
// pinned to the caller's project folder, output is byte-capped, and the child
// is killed past a timeout.

const EXEC_MAX_OUTPUT_BYTES: usize = 1024 * 1024; // 1 MiB per stream
const EXEC_DEFAULT_TIMEOUT_SECS: u64 = 120;
const EXEC_MAX_TIMEOUT_SECS: u64 = 600;

/// Executables a plugin may run. Bare names only — resolved via `PATH` by the
/// OS. Kept to version control, package managers, build drivers, and test
/// runners; nothing that reads arbitrary shell input.
const EXEC_ALLOWLIST: &[&str] = &[
    "git", "cargo", "rustc", "npm", "npx", "node", "pnpm", "yarn", "deno", "bun", "python",
    "python3", "pytest", "tox", "go", "make", "just", "bazel", "gradle", "mvn", "dotnet",
    "phpunit", "composer", "bundle", "rake", "rspec", "ruby", "jest", "vitest", "mocha", "tsc",
    "eslint", "prettier", "ruff", "mypy", "flake8", "ctest", "cmake", "ant", "swift", "dart",
    "flutter",
];

#[derive(Deserialize)]
struct ExecRequest {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// Drain a child pipe into a byte-capped buffer on its own thread. Reading to
/// EOF (even past the cap, discarding the overflow) keeps the child from
/// blocking on a full pipe. Returns `(bytes, truncated)`.
fn drain_capped<R: std::io::Read + Send + 'static>(
    mut r: R,
) -> std::thread::JoinHandle<(Vec<u8>, bool)> {
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut truncated = false;
        let mut buf = [0u8; 8192];
        loop {
            match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if out.len() < EXEC_MAX_OUTPUT_BYTES {
                        let room = EXEC_MAX_OUTPUT_BYTES - out.len();
                        if n > room {
                            out.extend_from_slice(&buf[..room]);
                            truncated = true;
                        } else {
                            out.extend_from_slice(&buf[..n]);
                        }
                    } else {
                        truncated = true;
                    }
                }
                Err(_) => break,
            }
        }
        (out, truncated)
    })
}

/// `peckboard_exec` — run an allowlisted command in the caller's project
/// folder. Input: `{"command", "args"?: [..], "timeout_secs"?}`. Output:
/// `{"exit_code", "stdout", "stderr", "stdout_truncated", "stderr_truncated",
/// "timed_out"}` or an `{"error"}` envelope.
pub(crate) fn exec_impl(
    db: &Db,
    input: &str,
    inv: &InvocationContext,
    enforce_allowlist: bool,
) -> String {
    let req: ExecRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    let command = req.command.trim();
    if command.is_empty() {
        return error_json("command is required");
    }
    // Bare executable name only — no path component, no shell metacharacters.
    // This holds even for the unrestricted variant: args are an argv array, so
    // there is never a shell to interpret metacharacters, and the program is
    // resolved by name via PATH inside the folder-pinned cwd.
    if command.contains('/')
        || command.contains('\\')
        || command.contains(|c: char| c.is_whitespace())
    {
        return error_json("command must be a bare executable name");
    }
    if enforce_allowlist && !EXEC_ALLOWLIST.contains(&command) {
        return error_json(format!(
            "command '{command}' is not on the allowlist; permitted: {}",
            EXEC_ALLOWLIST.join(", ")
        ));
    }
    let root = match caller_folder_root(db, inv) {
        Ok(r) => r,
        Err(e) => return error_json(e),
    };
    let timeout = Duration::from_secs(
        req.timeout_secs
            .unwrap_or(EXEC_DEFAULT_TIMEOUT_SECS)
            .clamp(1, EXEC_MAX_TIMEOUT_SECS),
    );

    use std::process::{Command, Stdio};
    let mut child = match Command::new(command)
        .args(&req.args)
        .current_dir(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return error_json(format!("failed to start '{command}': {e}")),
    };

    let stdout_h = child.stdout.take().map(drain_capped);
    let stderr_h = child.stderr.take().map(drain_capped);

    // Poll for exit, killing the child if it overruns the timeout.
    let start = std::time::Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return error_json(format!("wait failed: {e}")),
        }
    };

    let (stdout, stdout_truncated) = stdout_h
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    let (stderr, stderr_truncated) = stderr_h
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();

    serde_json::json!({
        "exit_code": status.and_then(|s| s.code()),
        "stdout": String::from_utf8_lossy(&stdout),
        "stderr": String::from_utf8_lossy(&stderr),
        "stdout_truncated": stdout_truncated,
        "stderr_truncated": stderr_truncated,
        "timed_out": timed_out,
    })
    .to_string()
}

// ── Interactive user prompts (ask / read-answer) ──────────────────────
//
// `peckboard_ask_user` emits a single-question prompt to the caller's own
// session (via the `LiveHost` seam, which broadcasts so the UI renders it
// live), carrying an opaque `token`. The worker's turn then ends; when the
// user answers, core resumes the session. On the resumed turn the plugin calls
// `peckboard_get_answer` with the same `token` to read the user's *real* answer
// out of the session's event log — core is the source of truth, so the agent
// can't forge an approval. This is the substrate for the common-tools
// `run_command` per-command approval flow.

#[derive(Deserialize)]
struct AskUserRequest {
    question: String,
    #[serde(default)]
    options: Vec<String>,
    token: String,
    /// Optional explicit target: a session visible to the caller. Defaults
    /// to the caller's own session (the MCP invocation's).
    #[serde(default)]
    session_id: Option<String>,
    /// Optional: session the user's ANSWER should resume (instead of the
    /// session carrying the question). Must be visible to the caller.
    #[serde(default)]
    redirect_session_id: Option<String>,
}

#[derive(Deserialize)]
struct GetAnswerRequest {
    token: String,
    /// Optional explicit target: the session carrying the question — must be
    /// visible to the caller. Defaults to the caller's own session.
    #[serde(default)]
    session_id: Option<String>,
}

/// `peckboard_ask_user` — emit a prompt to the caller's session (or, with an
/// explicit `session_id`, to another session visible to the caller — e.g. the
/// pre-hatcher asking a clarifying question on the chat session it is
/// enriching). Returns `{"ok": true}` (fire-and-forget) or an error if there
/// is no target session / no live host bound (headless).
pub(crate) fn ask_user_impl(
    db: &Db,
    inv: &InvocationContext,
    input: &str,
    live: Option<Arc<dyn LiveHost>>,
) -> String {
    let req: AskUserRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    let session_id = match req.session_id.as_deref() {
        Some(sid) => match fetch_visible_session(db, sid.trim(), inv) {
            Ok(s) => s.id,
            Err(e) => return error_json(e),
        },
        None => match inv.session_id.clone() {
            Some(s) => s,
            None => {
                return error_json(
                    "no caller session; pass session_id or call during an MCP invocation",
                );
            }
        },
    };
    let redirect = match req.redirect_session_id.as_deref() {
        Some(rid) => match fetch_visible_session(db, rid.trim(), inv) {
            Ok(s) => Some(s.id),
            Err(e) => return error_json(e),
        },
        None => None,
    };
    if req.question.trim().is_empty() {
        return error_json("question is required");
    }
    if req.token.trim().is_empty() {
        return error_json("token is required");
    }
    let Some(live) = live else {
        return error_json("interactive prompts unavailable (no live host bound)");
    };
    live.ask_user(session_id, req.question, req.options, req.token, redirect);
    serde_json::json!({ "ok": true }).to_string()
}

/// `peckboard_get_answer` — resolve the answer to a plugin-emitted question
/// carrying `token` in the caller's session (or, with an explicit
/// `session_id`, another session visible to the caller). Returns
/// `{"status": "pending" | "answered" | "unknown", "answer"?, "rejected"?}`.
/// `unknown` means no question with that token exists for that session.
pub(crate) fn get_answer_impl(db: &Db, inv: &InvocationContext, input: &str) -> String {
    let req: GetAnswerRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    let session_id = match req.session_id.as_deref() {
        Some(sid) => match fetch_visible_session(db, sid.trim(), inv) {
            Ok(s) => s.id,
            Err(e) => return error_json(e),
        },
        None => match inv.session_id.clone() {
            Some(s) => s,
            None => {
                return error_json(
                    "no caller session; pass session_id or call during an MCP invocation",
                );
            }
        },
    };
    let events = match db.list_events_by_session_blocking(&session_id) {
        Ok(e) => e,
        Err(e) => return error_json(e.to_string()),
    };

    // Find the question event carrying this token (our own, in this session).
    let mut question_id: Option<String> = None;
    for e in &events {
        if e.kind == "question"
            && let Ok(d) = serde_json::from_str::<serde_json::Value>(&e.data)
            && d.get("approval_token").and_then(|v| v.as_str()) == Some(req.token.as_str())
        {
            question_id = Some(e.id.clone());
            break;
        }
    }
    let Some(qid) = question_id else {
        return serde_json::json!({ "status": "unknown" }).to_string();
    };

    // Find its resolution, if the user has answered yet.
    for e in &events {
        if e.kind != "question-resolved" {
            continue;
        }
        let Ok(d) = serde_json::from_str::<serde_json::Value>(&e.data) else {
            continue;
        };
        let resolved_for = d
            .get("question_id")
            .or_else(|| d.get("questionId"))
            .and_then(|v| v.as_str());
        if resolved_for != Some(qid.as_str()) {
            continue;
        }
        let rejected = d.get("rejected").and_then(|v| v.as_bool()).unwrap_or(false);
        // Our prompt is a single question, so the chosen label is answers["0"].
        let answer = d
            .get("answers")
            .and_then(|a| a.get("0"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return serde_json::json!({
            "status": "answered",
            "rejected": rejected,
            "answer": answer,
        })
        .to_string();
    }
    serde_json::json!({ "status": "pending" }).to_string()
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

host_fn!(peckboard_update_card(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok) = state_and_permission(&user_data, "cards_write")?;
    if !ok { return Ok(error_json("plugin lacks the 'cards_write' permission")); }
    Ok(update_card_impl(&db, &input))
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
    let (db, plugin_id, ok, inv, live) = state_permission_invocation_and_live(&user_data, "session_write")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_write' permission")); }
    let Some(inv) = inv else { return Ok(error_json("no caller context; peckboard_update_session is only callable during a tool invocation")); };
    Ok(update_session_impl(&db, &plugin_id, &input, &inv, live))
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

host_fn!(peckboard_read_file_base64(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok, inv) = state_permission_and_invocation(&user_data, "project_files_read")?;
    if !ok { return Ok(error_json("plugin lacks the 'project_files_read' permission")); }
    let Some(inv) = inv else { return Ok(error_json("no caller context; peckboard_read_file_base64 is only callable during a tool invocation")); };
    Ok(read_file_base64_impl(&db, &input, &inv))
});

host_fn!(peckboard_write_file(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok, inv) = state_permission_and_invocation(&user_data, "project_files_write")?;
    if !ok { return Ok(error_json("plugin lacks the 'project_files_write' permission")); }
    let Some(inv) = inv else { return Ok(error_json("no caller context; peckboard_write_file is only callable during a tool invocation")); };
    Ok(write_file_impl(&db, &input, &inv))
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

host_fn!(peckboard_deliver_message(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok, inv, live) = state_permission_invocation_and_live(&user_data, "session_dispatch")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_dispatch' permission")); }
    let Some(inv) = inv else { return Ok(error_json("no caller context; peckboard_deliver_message is only callable during a tool invocation")); };
    Ok(deliver_message_impl(&db, &input, &inv, live))
});

// Session control: full cross-folder control of any session. Gated on the
// `session_control` permission; no invocation-context boundary (the operator
// grants this by approving the plugin).
host_fn!(peckboard_interrupt_session(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok, _inv, live) = state_permission_invocation_and_live(&user_data, "session_control")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_control' permission")); }
    Ok(interrupt_session_impl(&db, &input, live))
});

host_fn!(peckboard_terminate_agent(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok, _inv, live) = state_permission_invocation_and_live(&user_data, "session_control")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_control' permission")); }
    Ok(terminate_agent_impl(&db, &input, live))
});

host_fn!(peckboard_clear_session(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok, _inv, live) = state_permission_invocation_and_live(&user_data, "session_control")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_control' permission")); }
    Ok(clear_session_impl(&db, &input, live))
});

host_fn!(peckboard_send_message(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok, _inv, live) = state_permission_invocation_and_live(&user_data, "session_control")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_control' permission")); }
    Ok(send_message_impl(&db, &input, live))
});

host_fn!(peckboard_list_all_sessions(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok) = state_and_permission(&user_data, "session_control")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_control' permission")); }
    Ok(list_all_sessions_impl(&db, &input))
});

host_fn!(peckboard_http_fetch(user_data: HostState; input: String) -> String {
    let (_db, _plugin_id, ok) = state_and_permission(&user_data, "http_fetch")?;
    if !ok { return Ok(error_json("plugin lacks the 'http_fetch' permission")); }
    Ok(http_fetch_impl(&input))
});

host_fn!(peckboard_http_request(user_data: HostState; input: String) -> String {
    let (_db, _plugin_id, ok) = state_and_permission(&user_data, "http_request")?;
    if !ok { return Ok(error_json("plugin lacks the 'http_request' permission")); }
    Ok(http_request_impl(&input))
});
host_fn!(peckboard_exec(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok, inv) = state_permission_and_invocation(&user_data, "process_exec")?;
    if !ok { return Ok(error_json("plugin lacks the 'process_exec' permission")); }
    let Some(inv) = inv else { return Ok(error_json("no caller context; peckboard_exec is only callable during a tool invocation")); };
    Ok(exec_impl(&db, &input, &inv, true))
});

host_fn!(peckboard_exec_any(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok, inv) = state_permission_and_invocation(&user_data, "process_exec_any")?;
    if !ok { return Ok(error_json("plugin lacks the 'process_exec_any' permission")); }
    let Some(inv) = inv else { return Ok(error_json("no caller context; peckboard_exec_any is only callable during a tool invocation")); };
    Ok(exec_impl(&db, &input, &inv, false))
});

host_fn!(peckboard_ask_user(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok, inv, live) = state_permission_invocation_and_live(&user_data, "ask_user")?;
    if !ok { return Ok(error_json("plugin lacks the 'ask_user' permission")); }
    let Some(inv) = inv else { return Ok(error_json("no caller context; peckboard_ask_user is only callable during a tool invocation")); };
    Ok(ask_user_impl(&db, &inv, &input, live))
});

host_fn!(peckboard_get_answer(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok, inv) = state_permission_and_invocation(&user_data, "ask_user")?;
    if !ok { return Ok(error_json("plugin lacks the 'ask_user' permission")); }
    let Some(inv) = inv else { return Ok(error_json("no caller context; peckboard_get_answer is only callable during a tool invocation")); };
    Ok(get_answer_impl(&db, &inv, &input))
});

/// Shared accessor for the browser-run host functions: permission check +
/// the app data dir where `service::browser_runs` records runs.
fn state_permission_and_data_dir(
    user_data: &UserData<HostState>,
    permission: &str,
) -> Result<(bool, std::path::PathBuf), Error> {
    let state = user_data.get()?;
    let state = state
        .lock()
        .map_err(|_| anyhow::anyhow!("plugin host state mutex poisoned"))?;
    let ok = state
        .permissions
        .read()
        .map(|p| p.contains(permission))
        .unwrap_or(false);
    Ok((ok, state.data_dir.clone()))
}

// `peckboard_browser_runs` — list recorded browser test runs (newest first,
// steps included; frame bytes fetched separately). Gated by
// `browser_runs_read`.
host_fn!(peckboard_browser_runs(user_data: HostState; _input: String) -> String {
    let (ok, data_dir) = state_permission_and_data_dir(&user_data, "browser_runs_read")?;
    if !ok { return Ok(error_json("plugin lacks the 'browser_runs_read' permission")); }
    let runs = crate::service::browser_runs::list_runs(&data_dir);
    Ok(serde_json::json!({ "runs": runs }).to_string())
});

// `peckboard_browser_run` — one run's full meta. `{run_id}` → `{run}`.
host_fn!(peckboard_browser_run(user_data: HostState; input: String) -> String {
    let (ok, data_dir) = state_permission_and_data_dir(&user_data, "browser_runs_read")?;
    if !ok { return Ok(error_json("plugin lacks the 'browser_runs_read' permission")); }
    let run_id = serde_json::from_str::<serde_json::Value>(&input)
        .ok()
        .and_then(|v| v.get("run_id").and_then(|r| r.as_str()).map(str::to_string))
        .unwrap_or_default();
    match crate::service::browser_runs::get_run(&data_dir, &run_id) {
        Some(run) => Ok(serde_json::json!({ "run": run }).to_string()),
        None => Ok(error_json("run not found")),
    }
});

// `peckboard_browser_run_frame` — one frame's PNG bytes as base64.
// `{run_id, frame}` → `{base64}`.
host_fn!(peckboard_browser_run_frame(user_data: HostState; input: String) -> String {
    let (ok, data_dir) = state_permission_and_data_dir(&user_data, "browser_runs_read")?;
    if !ok { return Ok(error_json("plugin lacks the 'browser_runs_read' permission")); }
    let v = serde_json::from_str::<serde_json::Value>(&input).unwrap_or_default();
    let run_id = v.get("run_id").and_then(|r| r.as_str()).unwrap_or_default();
    let frame = v.get("frame").and_then(|r| r.as_str()).unwrap_or_default();
    match crate::service::browser_runs::get_frame(&data_dir, run_id, frame) {
        Some(base64) => Ok(serde_json::json!({ "base64": base64 }).to_string()),
        None => Ok(error_json("frame not found")),
    }
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
    data_dir: std::path::PathBuf,
) -> Vec<Function> {
    let ud = UserData::new(HostState {
        db: db.clone(),
        data_dir,
        plugin_id: plugin_id.to_string(),
        permissions,
        invocation,
        live,
        user,
    });
    vec![
        Function::new(
            "peckboard_browser_runs",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_browser_runs,
        ),
        Function::new(
            "peckboard_browser_run",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_browser_run,
        ),
        Function::new(
            "peckboard_browser_run_frame",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_browser_run_frame,
        ),
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
            "peckboard_update_card",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_update_card,
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
            "peckboard_read_file_base64",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_read_file_base64,
        ),
        Function::new(
            "peckboard_write_file",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_write_file,
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
            ud.clone(),
            peckboard_resume_session,
        ),
        Function::new(
            "peckboard_deliver_message",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_deliver_message,
        ),
        Function::new(
            "peckboard_interrupt_session",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_interrupt_session,
        ),
        Function::new(
            "peckboard_terminate_agent",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_terminate_agent,
        ),
        Function::new(
            "peckboard_clear_session",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_clear_session,
        ),
        Function::new(
            "peckboard_send_message",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_send_message,
        ),
        Function::new(
            "peckboard_list_all_sessions",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_list_all_sessions,
        ),
        Function::new(
            "peckboard_http_fetch",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_http_fetch,
        ),
        Function::new(
            "peckboard_http_request",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_http_request,
        ),
        Function::new("peckboard_exec", [PTR], [PTR], ud.clone(), peckboard_exec),
        Function::new(
            "peckboard_exec_any",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_exec_any,
        ),
        Function::new(
            "peckboard_ask_user",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_ask_user,
        ),
        Function::new(
            "peckboard_get_answer",
            [PTR],
            [PTR],
            ud,
            peckboard_get_answer,
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

    #[tokio::test]
    async fn session_control_impls_validate_target_and_live() {
        let db = Db::in_memory().unwrap();
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "f1".into(),
            path: "/tmp/f1".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_session(crate::db::models::NewSession {
            id: "s1".into(),
            name: "s1".into(),
            folder_id: "f1".into(),
            created_at: ts.clone(),
            last_activity: ts,
            ..Default::default()
        })
        .await
        .unwrap();

        // Unknown target id → "not found" (no boundary check, just existence).
        let nf = interrupt_session_impl(&db, r#"{"session_id":"nope"}"#, None);
        assert!(nf.contains("not found"), "{nf}");

        // Known session, but no live host wired → reports unavailable, not a panic.
        let nl = clear_session_impl(&db, r#"{"session_id":"s1"}"#, None);
        assert!(nl.contains("live control unavailable"), "{nl}");

        // send_message refuses an empty payload (no text, no attachments).
        let empty = send_message_impl(&db, r#"{"session_id":"s1","text":"  "}"#, None);
        assert!(empty.contains("requires"), "{empty}");

        // Malformed base64 attachment is rejected before dispatch.
        let bad = send_message_impl(
            &db,
            r#"{"session_id":"s1","text":"hi","attachments":[{"filename":"a.png","mime_type":"image/png","data_base64":"!notbase64!"}]}"#,
            None,
        );
        assert!(bad.contains("invalid base64"), "{bad}");
    }

    #[tokio::test]
    async fn create_session_impl_inherits_caller_owner() {
        let db = Db::in_memory().unwrap();
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_user(crate::db::models::NewUser {
            id: "u1".into(),
            username: "u1".into(),
            email: None,
            password_hash: "h".into(),
            role: "user".into(),
            created_at: ts.clone(),
            updated_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "f1".into(),
            path: "/tmp/f1".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        // Caller (e.g. an agent spinning up an expert) owned by u1.
        db.create_session(crate::db::models::NewSession {
            id: "caller".into(),
            name: "caller".into(),
            folder_id: "f1".into(),
            created_at: ts.clone(),
            last_activity: ts.clone(),
            user_id: Some("u1".into()),
            ..Default::default()
        })
        .await
        .unwrap();

        let caller = InvocationContext {
            session_id: Some("caller".into()),
            project_id: None,
            folder_id: Some("f1".into()),
            authority: false,
        };
        let out = create_session_impl(
            &db,
            r#"{"name":"expert: x","is_expert":true,"expert_kind":"pm"}"#,
            &caller,
        );
        let sid = serde_json::from_str::<serde_json::Value>(&out).unwrap()["session"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let spawned = db.get_session(&sid).await.unwrap().unwrap();
        // Plugin/expert-spawned session inherits the caller's owner.
        assert_eq!(spawned.user_id.as_deref(), Some("u1"));
        assert!(spawned.is_expert);
    }

    fn inv(project: Option<&str>, folder: Option<&str>) -> InvocationContext {
        InvocationContext {
            session_id: None,
            project_id: project.map(str::to_string),
            folder_id: folder.map(str::to_string),
            authority: false,
        }
    }

    /// The full-authority context an authenticated user request resolves to.
    fn inv_user() -> InvocationContext {
        InvocationContext {
            session_id: None,
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
            budget_usd_cents: None,
            budget_period: None,
            worktree_isolation: false,
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
            budget_usd_cents: None,
            budget_period: None,
            worktree_isolation: false,
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

        // Opting in via the flags sets the core classification columns.
        let out2 = create_session_impl(
            &db,
            r#"{"name":"expert: ws","is_expert":true,"expert_kind":"knowledge"}"#,
            &caller,
        );
        let v2: serde_json::Value = serde_json::from_str(&out2).unwrap();
        assert!(v2.get("error").is_none(), "create flagged: {out2}");
        assert_eq!(v2["session"]["is_expert"], true);
        assert_eq!(v2["session"]["expert_kind"], "knowledge");
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
            None,
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

        // The base64 variant returns the raw bytes intact (decodes back to the
        // file content) and is bound by the same containment checks.
        {
            use base64::Engine as _;
            let b = read_file_base64_impl(&db, r#"{"path":"src/main.rs"}"#, &caller);
            let bv: serde_json::Value = serde_json::from_str(&b).unwrap();
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(bv["base64"].as_str().unwrap())
                .unwrap();
            assert!(
                String::from_utf8_lossy(&decoded).contains("fn main"),
                "base64 read: {b}"
            );
            let esc64 = read_file_base64_impl(&db, r#"{"path":"../../etc/passwd"}"#, &caller);
            assert!(
                esc64.contains("within the project folder"),
                "base64 escape: {esc64}"
            );
            // A caller with no folder scope can read nothing through the base64
            // path — it is bound to the caller's project/session folder.
            let no_scope =
                read_file_base64_impl(&db, r#"{"path":"src/main.rs"}"#, &inv(Some("p1"), None));
            assert!(
                no_scope.contains("no folder scope"),
                "base64 requires folder scope: {no_scope}"
            );
        }

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
            // The base64 variant shares the same containment: a symlink that
            // escapes the folder is refused there too (no folder-scoped read of
            // out-of-folder bytes via the base64 path either).
            let leak64 = read_file_base64_impl(&db, r#"{"path":"link.txt"}"#, &caller);
            assert!(
                leak64.contains("escapes the project folder"),
                "base64 symlink escape must be refused: {leak64}"
            );
            let _ = fs::remove_file(&secret);
        }
    }

    #[tokio::test]
    async fn write_file_is_contained_and_roundtrips() {
        use std::fs;
        let db = Db::in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "fW".into(),
            name: "Repo".into(),
            path: root.to_string_lossy().to_string(),
            created_at: ts,
        })
        .await
        .unwrap();
        let caller = inv(Some("pW"), Some("fW"));

        // Create a new file in a new subdir, then read it back via read_file.
        let w = write_file_impl(
            &db,
            r#"{"path":"src/new.txt","content":"hello","create_dirs":true}"#,
            &caller,
        );
        assert!(w.contains("\"ok\":true"), "write: {w}");
        assert_eq!(
            fs::read_to_string(root.join("src/new.txt")).unwrap(),
            "hello"
        );

        // Append.
        let a = write_file_impl(
            &db,
            r#"{"path":"src/new.txt","content":" world","append":true}"#,
            &caller,
        );
        assert!(a.contains("\"ok\":true"), "append: {a}");
        assert_eq!(
            fs::read_to_string(root.join("src/new.txt")).unwrap(),
            "hello world"
        );

        // Overwrite (truncate).
        write_file_impl(&db, r#"{"path":"src/new.txt","content":"x"}"#, &caller);
        assert_eq!(fs::read_to_string(root.join("src/new.txt")).unwrap(), "x");

        // `..` traversal is refused before touching the fs.
        let esc = write_file_impl(&db, r#"{"path":"../escape.txt","content":"nope"}"#, &caller);
        assert!(esc.contains("within the project folder"), "escape: {esc}");
        assert!(!root.parent().unwrap().join("escape.txt").exists());

        // Missing parent without create_dirs is a clean error, not a write.
        let miss = write_file_impl(
            &db,
            r#"{"path":"deep/dir/file.txt","content":"x"}"#,
            &caller,
        );
        assert!(miss.contains("parent directory"), "missing parent: {miss}");

        // A symlinked intermediate dir pointing outside the folder is refused.
        #[cfg(unix)]
        {
            let outside = dir.path().parent().unwrap().join("outside_dir");
            fs::create_dir_all(&outside).unwrap();
            std::os::unix::fs::symlink(&outside, root.join("link_dir")).unwrap();
            let leak = write_file_impl(
                &db,
                r#"{"path":"link_dir/escaped.txt","content":"leak"}"#,
                &caller,
            );
            assert!(
                leak.contains("escapes the project folder"),
                "symlink escape must be refused: {leak}"
            );
            assert!(!outside.join("escaped.txt").exists());
            let _ = fs::remove_dir_all(&outside);
        }
    }

    #[test]
    fn blocked_fetch_ips_cover_private_and_special_ranges() {
        use std::net::{Ipv4Addr, Ipv6Addr};
        let blocked = [
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),       // loopback
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)),        // private
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),     // private
            IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1)),      // private
            IpAddr::V4(Ipv4Addr::new(169, 254, 0, 1)),     // link-local
            IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1)),      // CGNAT
            IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),         // unspecified
            IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)), // cloud metadata
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V6("fc00::1".parse().unwrap()), // unique-local
            IpAddr::V6("fe80::1".parse().unwrap()), // link-local
            IpAddr::V6("::ffff:10.0.0.1".parse().unwrap()), // v4-mapped private
        ];
        for ip in blocked {
            assert!(is_blocked_fetch_ip(&ip), "should block {ip}");
        }
        let allowed = [
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
            IpAddr::V6("2606:4700:4700::1111".parse().unwrap()),
        ];
        for ip in allowed {
            assert!(!is_blocked_fetch_ip(&ip), "should allow {ip}");
        }
    }

    #[test]
    fn http_fetch_rejects_bad_scheme_method_and_private_host() {
        // Non-http scheme.
        let r = http_fetch_impl(r#"{"url":"file:///etc/passwd"}"#);
        assert!(r.contains("http and https"), "scheme: {r}");
        // Disallowed method.
        let r = http_fetch_impl(r#"{"url":"https://example.com","method":"POST"}"#);
        assert!(r.contains("GET and HEAD"), "method: {r}");
        // Host that resolves only to loopback is refused (no network needed).
        let r = http_fetch_impl(r#"{"url":"http://localhost/"}"#);
        assert!(
            r.contains("public address") || r.contains("dns resolution"),
            "localhost: {r}"
        );
    }

    #[test]
    fn http_request_validates_method_and_reaches_local_targets() {
        // Non-http scheme refused.
        let r = http_request_impl(r#"{"url":"file:///etc/passwd"}"#);
        assert!(r.contains("http and https"), "scheme: {r}");
        // Unsupported method refused.
        let r = http_request_impl(r#"{"url":"http://example.com","method":"TRACE"}"#);
        assert!(r.contains("not permitted"), "method: {r}");
        // Loopback is the point of this host fn: a POST to a local listener
        // round-trips, response headers included.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut sock, _) = listener.accept().unwrap();
            let mut data = Vec::new();
            let mut buf = [0u8; 1024];
            loop {
                let n = sock.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                data.extend_from_slice(&buf[..n]);
                if String::from_utf8_lossy(&data).contains("\"jsonrpc\"") {
                    break;
                }
            }
            sock.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nmcp-session-id: s-1\r\nconnection: close\r\n\r\nok",
            )
            .unwrap();
            String::from_utf8_lossy(&data).into_owned()
        });
        let input = serde_json::json!({
            "url": format!("http://127.0.0.1:{}/api/mcp", addr.port()),
            "method": "POST",
            "headers": {"content-type": "application/json"},
            "body": "{\"jsonrpc\":\"2.0\"}",
            "timeout_secs": 5,
        });
        let r = http_request_impl(&input.to_string());
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["status"], 200, "response: {r}");
        assert_eq!(v["body"], "ok");
        assert_eq!(v["headers"]["mcp-session-id"], "s-1");
        let seen = server.join().unwrap();
        assert!(seen.starts_with("POST /api/mcp"), "server saw: {seen}");
        assert!(
            seen.contains("{\"jsonrpc\":\"2.0\"}"),
            "body forwarded: {seen}"
        );
    }

    #[tokio::test]
    async fn exec_enforces_allowlist_and_folder_scope() {
        let db = Db::in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "fE".into(),
            name: "Repo".into(),
            path: dir.path().to_string_lossy().to_string(),
            created_at: ts,
        })
        .await
        .unwrap();
        let caller = inv(Some("pE"), Some("fE"));

        // Not on the allowlist → refused before spawning (allowlist enforced).
        let r = exec_impl(&db, r#"{"command":"rm","args":["-rf","/"]}"#, &caller, true);
        assert!(r.contains("not on the allowlist"), "rm: {r}");

        // The unrestricted variant skips the allowlist (but still bare-name +
        // folder-scoped): `rm` is no longer refused on allowlist grounds.
        let r = exec_impl(
            &db,
            r#"{"command":"rm","args":["--version"]}"#,
            &caller,
            false,
        );
        assert!(
            !r.contains("not on the allowlist"),
            "exec_any allowlist: {r}"
        );

        // A path component (escape attempt) → refused as not-a-bare-name, even
        // for the unrestricted variant.
        let r = exec_impl(&db, r#"{"command":"../../bin/sh"}"#, &caller, false);
        assert!(r.contains("bare executable name"), "path: {r}");

        // No folder scope → refused (cwd cannot be pinned).
        let unscoped = inv(Some("pE"), None);
        let r = exec_impl(
            &db,
            r#"{"command":"git","args":["--version"]}"#,
            &unscoped,
            true,
        );
        assert!(r.contains("folder"), "unscoped: {r}");

        // Allowlisted command runs in the folder when the tool is present.
        let r = exec_impl(
            &db,
            r#"{"command":"git","args":["--version"]}"#,
            &caller,
            true,
        );
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        if v.get("error").is_none() {
            // git is installed: it ran and exited cleanly.
            assert_eq!(v["timed_out"], serde_json::json!(false), "exec: {r}");
            assert!(
                v["stdout"].as_str().unwrap_or("").contains("git")
                    || v["exit_code"] == serde_json::json!(0),
                "exec: {r}"
            );
        }
    }

    /// A plugin-driven model/effort change must recycle the session's live
    /// agent process — it keeps its spawn-time model and account credentials,
    /// so reusing it would answer (and bill) as the old model/account.
    /// Unrelated updates and no-op writes must not recycle.
    #[tokio::test]
    async fn update_session_model_change_recycles_agent() {
        let db = setup().await; // folder f1 / project p1
        let pid = "experts";
        let caller = inv(Some("p1"), Some("f1"));

        let sid = serde_json::from_str::<serde_json::Value>(&create_session_impl(
            &db,
            r#"{"name":"expert: m"}"#,
            &caller,
        ))
        .unwrap()["session"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        session_meta_set_impl(
            &db,
            pid,
            &format!(r#"{{"session_id":"{sid}","data":{{}}}}"#),
        );

        let rec = Arc::new(RecordingLive::default());

        // Name-only update: no recycle.
        let r = update_session_impl(
            &db,
            pid,
            &format!(r#"{{"session_id":"{sid}","name":"renamed"}}"#),
            &caller,
            Some(rec.clone()),
        );
        assert!(r.contains("renamed"), "update: {r}");
        assert!(
            rec.calls.lock().unwrap().is_empty(),
            "name-only update must not recycle"
        );

        // Model change: the live child is recycled.
        let r = update_session_impl(
            &db,
            pid,
            &format!(r#"{{"session_id":"{sid}","model":"claude:claude-fable-5"}}"#),
            &caller,
            Some(rec.clone()),
        );
        assert!(r.contains("claude-fable-5"), "update: {r}");
        assert_eq!(
            rec.calls.lock().unwrap().as_slice(),
            [format!("recycle:{sid}")]
        );

        // Writing the same model again is not a change: no second recycle.
        let r = update_session_impl(
            &db,
            pid,
            &format!(r#"{{"session_id":"{sid}","model":"claude:claude-fable-5"}}"#),
            &caller,
            Some(rec.clone()),
        );
        assert!(r.contains("session"), "update: {r}");
        assert_eq!(
            rec.calls.lock().unwrap().len(),
            1,
            "unchanged model must not recycle"
        );

        // Effort change: recycles too (effort rides the spawn config).
        let r = update_session_impl(
            &db,
            pid,
            &format!(r#"{{"session_id":"{sid}","effort":"high"}}"#),
            &caller,
            Some(rec.clone()),
        );
        assert!(r.contains("high"), "update: {r}");
        assert_eq!(rec.calls.lock().unwrap().len(), 2);
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
        fn ask_user(
            &self,
            session_id: String,
            _q: String,
            _o: Vec<String>,
            token: String,
            _redirect: Option<String>,
        ) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("ask:{session_id}:{token}"));
        }
        fn recycle_agent_after_turn(&self, session_id: String) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("recycle:{session_id}"));
        }
    }

    #[tokio::test]
    async fn ask_user_and_get_answer_roundtrip() {
        let db = Db::in_memory().unwrap();
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "fA".into(),
            name: "Repo".into(),
            path: ".".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_session(crate::db::models::NewSession {
            id: "sA".into(),
            name: "Caller".into(),
            folder_id: "fA".into(),
            project_id: None,
            is_worker: true,
            created_at: ts.clone(),
            last_activity: ts,
            ..Default::default()
        })
        .await
        .unwrap();

        let ctx = InvocationContext {
            session_id: Some("sA".into()),
            project_id: None,
            folder_id: Some("fA".into()),
            authority: false,
        };

        // No question with this token yet → unknown.
        let r = get_answer_impl(&db, &ctx, r#"{"token":"tok1"}"#);
        assert!(r.contains("\"unknown\""), "unknown: {r}");

        // ask_user with no live host → error; with a live host → ok + recorded.
        let no_live = ask_user_impl(
            &db,
            &ctx,
            r#"{"question":"run rg?","options":["yes"],"token":"tok1"}"#,
            None,
        );
        assert!(no_live.contains("error"), "no live: {no_live}");
        let rec = std::sync::Arc::new(RecordingLive::default());
        let ok = ask_user_impl(
            &db,
            &ctx,
            r#"{"question":"run rg?","options":["Approve once","Approve always","Deny"],"token":"tok1"}"#,
            Some(rec.clone()),
        );
        assert!(ok.contains("\"ok\":true"), "ask ok: {ok}");
        assert!(
            rec.calls.lock().unwrap().iter().any(|c| c == "ask:sA:tok1"),
            "ask recorded: {:?}",
            rec.calls.lock().unwrap()
        );

        // The test live host doesn't actually emit the event, so seed the
        // question the real AppLiveHost would write, carrying the token.
        db.append_event_blocking(
            "sA",
            "question",
            r#"{"approval_token":"tok1","questions":[{"question":"run rg?"}]}"#,
        )
        .unwrap();
        // Now pending (asked, not yet answered).
        let r = get_answer_impl(&db, &ctx, r#"{"token":"tok1"}"#);
        assert!(r.contains("\"pending\""), "pending: {r}");

        // User answers → question-resolved referencing the question event id.
        let qid = db
            .list_events_by_session_blocking("sA")
            .unwrap()
            .into_iter()
            .find(|e| e.kind == "question")
            .unwrap()
            .id;
        db.append_event_blocking(
            "sA",
            "question-resolved",
            &format!(r#"{{"question_id":"{qid}","answers":{{"0":"Approve always"}}}}"#),
        )
        .unwrap();
        let r = get_answer_impl(&db, &ctx, r#"{"token":"tok1"}"#);
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["status"], "answered", "answered: {r}");
        assert_eq!(v["answer"], "Approve always", "answer: {r}");
        assert_eq!(v["rejected"], serde_json::json!(false));

        // A caller without a session context cannot read answers.
        let no_sess = InvocationContext::default();
        let r = get_answer_impl(&db, &no_sess, r#"{"token":"tok1"}"#);
        assert!(r.contains("error"), "no session: {r}");
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
            budget_usd_cents: None,
            budget_period: None,
            worktree_isolation: false,
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
            budget_usd_cents: None,
            budget_period: None,
            worktree_isolation: false,
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

        let out = create_card_impl(
            &db,
            &serde_json::json!({ "project_id": "p1", "title": "x", "effort": "very high" })
                .to_string(),
        );
        assert!(out.contains("invalid effort"), "got: {out}");

        // Canonical levels (incl. xhigh/max) pass.
        let out = create_card_impl(
            &db,
            &serde_json::json!({ "project_id": "p1", "title": "x", "effort": "xhigh" }).to_string(),
        );
        assert!(out.contains("\"card\""), "got: {out}");
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

    #[tokio::test]
    async fn update_card_succeeds_and_partial_leaves_other_fields() {
        let db = setup().await;

        // Create a card to update.
        let created = create_card_impl(
            &db,
            &serde_json::json!({
                "project_id": "p1",
                "title": "Original",
                "priority": 2,
                "step": "backlog"
            })
            .to_string(),
        );
        let cv: serde_json::Value = serde_json::from_str(&created).unwrap();
        assert!(cv.get("error").is_none(), "create error: {created}");
        let card_id = cv["card"]["id"].as_str().unwrap().to_string();

        // Update only the title; other fields must remain unchanged.
        let out = update_card_impl(
            &db,
            &serde_json::json!({ "card_id": card_id, "title": "Updated" }).to_string(),
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("error").is_none(), "update error: {out}");
        assert_eq!(v["card"]["title"], "Updated");
        assert_eq!(v["card"]["priority"], 2, "priority changed unexpectedly");
        assert_eq!(v["card"]["step"], "backlog", "step changed unexpectedly");

        // Update multiple fields at once.
        let out2 = update_card_impl(
            &db,
            &serde_json::json!({ "card_id": card_id, "priority": 1, "step": "in_progress" })
                .to_string(),
        );
        let v2: serde_json::Value = serde_json::from_str(&out2).unwrap();
        assert!(
            v2.get("error").is_none(),
            "multi-field update error: {out2}"
        );
        assert_eq!(v2["card"]["priority"], 1);
        assert_eq!(v2["card"]["step"], "in_progress");
        // Title was not touched in this call.
        assert_eq!(v2["card"]["title"], "Updated");
    }

    #[tokio::test]
    async fn update_card_unknown_id_is_error() {
        let db = setup().await;
        let out = update_card_impl(
            &db,
            &serde_json::json!({ "card_id": "no-such-card" }).to_string(),
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["error"], "card not found");
    }

    #[tokio::test]
    async fn update_card_invalid_priority_and_effort_are_errors() {
        let db = setup().await;
        let created = create_card_impl(
            &db,
            &serde_json::json!({ "project_id": "p1", "title": "x" }).to_string(),
        );
        let card_id = serde_json::from_str::<serde_json::Value>(&created).unwrap()["card"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        let bad_prio = update_card_impl(
            &db,
            &serde_json::json!({ "card_id": card_id, "priority": 99 }).to_string(),
        );
        assert!(bad_prio.contains("invalid priority"), "got: {bad_prio}");

        let bad_effort = update_card_impl(
            &db,
            &serde_json::json!({ "card_id": card_id, "effort": "very high" }).to_string(),
        );
        assert!(bad_effort.contains("invalid effort"), "got: {bad_effort}");
    }

    #[tokio::test]
    async fn update_card_malformed_json_is_error() {
        let db = setup().await;
        assert!(update_card_impl(&db, "not json").contains("invalid request"));
    }
}
