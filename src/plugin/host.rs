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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use extism::*;
use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::db::models::NewCard;
use crate::service::fs_jail;

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
    /// Host-side state of in-flight plugin-provider turns (stop flags,
    /// event guard, trusted session snapshots). Shared with every
    /// [`crate::provider::plugin_provider::PluginProviderAdapter`] the
    /// manager registers; the `peckboard_emit_provider_event` /
    /// `_provider_should_stop` / `_provider_get_*` host functions act on it.
    provider_runtime: Arc<crate::provider::plugin_provider::PluginProviderRuntime>,
    /// Staging slot for `peckboard_register_provider`: the host function
    /// shape-validates and parks the registration here; the manager applies
    /// it to the `ProviderRegistry` right after dispatching the plugin's
    /// `provider.register` hook (see `PluginManager::sync_plugin_providers`).
    pending_provider:
        Arc<std::sync::RwLock<Option<crate::provider::plugin_provider::ProviderRegistration>>>,
    /// Late-bound provider registry (see [`ProviderRegistrySlot`]);
    /// `peckboard_list_models` resolves the selectable model catalog from it.
    provider_registry: ProviderRegistrySlot,
}

/// Shared late-bound slot holding the app's provider registry. `Weak` because
/// the registry can hold plugin-provider adapters that point back into the
/// plugin layer — a strong ref here would cycle. Bound once by `main.rs` via
/// `PluginManager::set_provider_registry`; `None` until then (and in
/// registry-less tests), so `peckboard_list_models` refuses cleanly.
pub(crate) type ProviderRegistrySlot =
    Arc<std::sync::RwLock<Option<std::sync::Weak<crate::provider::registry::ProviderRegistry>>>>;

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
    /// Resolve a pending user question on `session_id` as `user_id`: persist
    /// the `question-resolved` event, broadcast it, feed question-expert
    /// plugins, and resume the answered conversation — the same flow as
    /// answering from core's own UI (see [`crate::service::questions`]).
    /// The caller has already authorized the target session and verified the
    /// question is real and unresolved. Fire-and-forget; no-op default.
    fn answer_question(
        &self,
        _session_id: String,
        _question_id: String,
        _answers: serde_json::Value,
        _rejected: bool,
        _user_id: String,
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

/// Proof token: the bearer's caller context came from one of the two trusted
/// slots — an in-flight `mcp.tool.invoke` (scope set host-side from the
/// verified MCP token), or an authenticated plugin-UI request
/// (`serve_http_authed`) made by a plugin granted `user_authority`. The
/// session-dispatch host functions take this instead of a bare
/// `&InvocationContext`, so no call path can hand them a plugin-derived
/// scope — only [`trusted_caller`] constructs one. See CLAUDE.md "Enforce
/// Critical Invariants in the Type System"; `SessionLock` in
/// `src/provider/manager.rs` is the sibling pattern.
pub(crate) struct TrustedCaller(InvocationContext);

impl std::ops::Deref for TrustedCaller {
    type Target = InvocationContext;

    /// One-way: a token yields its verified scope, but a scope never yields
    /// a token.
    fn deref(&self) -> &InvocationContext {
        &self.0
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

/// Folder metadata for pickers (id, name, path). Ungated like
/// `list_projects_impl` — folder rows carry no secrets and every loaded
/// plugin is already trusted to run in-process.
pub(crate) fn list_folders_impl(db: &Db) -> String {
    match db.list_folders_blocking() {
        Ok(folders) => {
            let folders: Vec<serde_json::Value> = folders
                .into_iter()
                .map(|f| serde_json::json!({ "id": f.id, "name": f.name, "path": f.path }))
                .collect();
            serde_json::json!({ "folders": folders }).to_string()
        }
        Err(e) => error_json(e),
    }
}

/// `peckboard_list_models` — the model catalog a plugin may offer for
/// session creation, across every non-hidden provider. THINKING MODELS ONLY,
/// filtered server-side: a non-thinking model must not be selectable at all,
/// so the filter is not left to callers. Reads the same registry that backs
/// `GET /api/models`, but through the dynamic∪static union
/// (`list_providers_with_models_union_except`) so a registry-known model a
/// stale external catalog hides — `claude-fable-5` behind an older `claude`
/// CLI — stays listed. Metadata only: id, display name, provider, account
/// id, tier. Never credentials, tokens, or account material.
pub(crate) fn list_models_impl(
    db: &Db,
    registry: Option<Arc<crate::provider::registry::ProviderRegistry>>,
) -> String {
    let Some(registry) = registry else {
        return error_json("provider registry unavailable");
    };
    let db = db.clone();
    // Host functions run synchronously inside a plugin call (often on a tokio
    // worker); catalog resolution is async. Same isolation as
    // `perform_outbound_http`: a dedicated thread with its own runtime.
    let result = std::thread::spawn(move || -> Result<serde_json::Value, String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("runtime: {e}"))?;
        rt.block_on(async move {
            let hidden = crate::routes::settings::hidden_providers_for_db(db).await;
            let providers = registry
                .list_providers_with_models_union_except(&hidden)
                .await;
            let mut models = Vec::new();
            for p in &providers {
                for m in &p.models {
                    if !m.is_thinking() {
                        continue;
                    }
                    let (_, account_id) = crate::provider::registry::split_model_account(&m.id);
                    models.push(serde_json::json!({
                        "id": format!("{}:{}", p.id, m.id),
                        "display_name": m.display_name,
                        "provider": p.id,
                        "account_id": account_id,
                        "thinking": true,
                        "tier": m.tier,
                    }));
                }
            }
            Ok(serde_json::json!({ "models": models }))
        })
    })
    .join();
    match result {
        Ok(Ok(v)) => v.to_string(),
        Ok(Err(e)) => error_json(e),
        Err(_) => error_json("model listing worker panicked"),
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
        worktree_unmerged_reason: None,
        worktree_unmerged_detail: None,
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

#[derive(Deserialize)]
struct SessionEventsRequest {
    session_id: String,
    #[serde(default)]
    after_seq: Option<i32>,
    #[serde(default)]
    limit: Option<i64>,
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

/// Read a slim tail of a session's event log for visualization: the `seq`,
/// `kind`, and (when the payload carries one) the tool `name` of each event
/// after `after_seq` (default 0 → from the beginning), oldest-first, capped at
/// `limit` (clamped to 1..=200, default 200). Deliberately omits the event
/// `data` payloads — it surfaces only low-sensitivity activity metadata (which
/// tool ran, in what order), never message text or tool arguments.
pub(crate) fn session_events_impl(db: &Db, input: &str) -> String {
    let req: SessionEventsRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    let after_seq = req.after_seq.unwrap_or(0);
    let limit = req.limit.unwrap_or(200).clamp(1, 200);
    match db.events_since_blocking(req.session_id.trim(), after_seq, limit) {
        Ok(events) => {
            let latest_seq = events.last().map(|e| e.seq);
            let items: Vec<serde_json::Value> = events
                .into_iter()
                .map(|e| {
                    // Slim by design: seq + kind + the tool name only, never `data`.
                    let name = serde_json::from_str::<serde_json::Value>(&e.data)
                        .ok()
                        .and_then(|d| d.get("name").and_then(|n| n.as_str()).map(String::from));
                    serde_json::json!({ "seq": e.seq, "kind": e.kind, "name": name })
                })
                .collect();
            serde_json::json!({ "events": items, "latest_seq": latest_seq }).to_string()
        }
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
    /// Create the session as a *temp* session (the UI deletes it when its
    /// tab closes), like the core install-session flow. Default false.
    #[serde(default)]
    is_temp: bool,
    /// Land the session in the folder registered at this filesystem path,
    /// registering the folder (and creating the directory) when missing —
    /// the server-side twin of the core UI's install-folder flow
    /// (`web/src/utils/installSession.ts`). A leading `~/` expands to the
    /// server user's home directory. Honored only when the caller holds
    /// full user authority (an authenticated plugin-UI request); a plugin
    /// MCP tool call stays pinned to its caller's folder (DESIGN §7.4).
    #[serde(default)]
    folder_path: Option<String>,
    /// Display name used if `folder_path` has to register the folder;
    /// defaults to the path's last segment.
    #[serde(default)]
    folder_name: Option<String>,
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
struct SetSessionSystemPromptRequest {
    session_id: String,
    /// A raw prompt body. Wins over `name` — a raw body has no library name,
    /// so the recorded reference is cleared along with it.
    #[serde(default)]
    system_prompt: Option<String>,
    /// A library prompt to resolve by name: its body is written AND the name
    /// recorded on the session, so the UI can show where the prompt came from.
    #[serde(default)]
    name: Option<String>,
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

/// Expand a leading `~` / `~/` in a plugin-supplied folder path. Pure (the
/// caller resolves `home` from HOME/USERPROFILE) so it is testable without
/// touching the process environment. Anything not `~`-prefixed must already
/// be absolute — a relative path would silently resolve against the server
/// process cwd.
fn expand_home_path(
    path: &str,
    home: Option<std::path::PathBuf>,
) -> Result<std::path::PathBuf, String> {
    if path == "~" || path.starts_with("~/") {
        let Some(home) = home else {
            return Err("folder_path starts with '~' but no home directory is resolvable".into());
        };
        let rest = path.trim_start_matches('~').trim_start_matches('/');
        return Ok(if rest.is_empty() {
            home
        } else {
            home.join(rest)
        });
    }
    let p = std::path::PathBuf::from(path);
    if !p.is_absolute() {
        return Err(format!(
            "folder_path must be absolute or start with '~/': {path}"
        ));
    }
    Ok(p)
}

fn resolve_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
}

/// Find-or-register the folder row for `folder_path`: the server-side twin
/// of the core UI's install-folder flow (`startInstallSession`). Applies the
/// same unsafe-path refusal as `POST /api/folders`, creates the directory,
/// and reuses any folder already registered at the exact expanded path.
fn folder_id_for_path(db: &Db, raw: &str, name: Option<&str>) -> Result<String, String> {
    let path = expand_home_path(raw, resolve_home())?;
    // An in-memory DB (tests) has no data dir; the system-path refusals
    // still apply via a placeholder no real path can live under.
    let data_dir = db
        .data_dir()
        .unwrap_or_else(|| std::path::Path::new("/nonexistent-peckboard-data-dir"));
    crate::routes::folders::reject_unsafe_path(&path, data_dir)?;
    std::fs::create_dir_all(&path).map_err(|e| {
        format!(
            "failed to create the folder directory {}: {e}",
            path.display()
        )
    })?;
    let path_str = path.to_string_lossy().to_string();
    if let Some(existing) = db
        .find_folder_by_path_blocking(&path_str)
        .map_err(|e| e.to_string())?
    {
        return Ok(existing.id);
    }
    let fallback = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("Sessions")
        .to_string();
    let name = name
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .unwrap_or(fallback);
    let folder = db
        .create_folder_blocking(crate::db::models::NewFolder {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            path: path_str,
            created_at: chrono::Utc::now().to_rfc3339(),
        })
        .map_err(|e| e.to_string())?;
    Ok(folder.id)
}
/// `peckboard_create_session` — create a generic session in the *caller's*
/// folder and project. Expert *knowledge* state is the plugin's own metadata
/// (`peckboard_session_meta_set`); the optional `is_expert`/`expert_kind`
/// flags only classify the session for usage attribution and listings. Takes
/// the [`TrustedCaller`] proof: reachable only from a tool invocation or an
/// authenticated `user_authority` plugin-UI request.
pub(crate) fn create_session_impl(db: &Db, input: &str, caller: &TrustedCaller) -> String {
    let inv: &InvocationContext = caller;
    let req: CreateSessionRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    if req.name.trim().is_empty() {
        return error_json("name is required");
    }
    let folder_id = match req
        .folder_path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        Some(path) => {
            if !inv.authority {
                return error_json(
                    "folder_path requires an authenticated user request; a tool invocation creates sessions in its caller's folder",
                );
            }
            match folder_id_for_path(db, path, req.folder_name.as_deref()) {
                Ok(id) => id,
                Err(e) => return error_json(e),
            }
        }
        None => match inv.folder_id.clone() {
            Some(id) => id,
            None => return error_json("caller has no folder scope; cannot create a session"),
        },
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
        handover_run_id: None,
        handover_to_model: None,
        pending_handover_doc: None,
        worker_step: None,
        // Inherit the caller session's owner (experts/plugin-spawned sessions);
        // falls back to the sole user, else NULL on multi-user installs.
        user_id: db.resolve_spawned_session_owner_blocking(inv.session_id.as_deref()),
        context_reset_ts: None,
        model_autoswitch: None,
        system_prompt_name: req.system_prompt_name,
        is_temp: req.is_temp,
        parent_session_id: None,
        subagent_completed_at: None,
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
        handover_run_id: None,
        handover_to_model: None,
        pending_handover_doc: None,
        worker_step: None,
        context_reset_ts: None,
        model_autoswitch: None,
        pending_plan_review: None,
        is_temp: None,
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

/// `peckboard_set_session_system_prompt` — set (or clear) the standing
/// instructions a session's agent runs under. Mirrors core's MCP tool of the
/// same name, down to the length cap and the error wording.
///
/// Authorized by **visibility only** (`fetch_visible_session`), not ownership:
/// the whole point of this capability is steering a session the plugin did not
/// create — e.g. graphify aiming an existing research session at its analyst
/// prompt — which is precisely the case `fetch_visible_session` exists for.
/// The boundary that still holds is "no cross-folder/project escalation".
///
/// An explicit `system_prompt` body wins (and clears the library reference,
/// since a raw body has no name); otherwise a library `name` resolves to that
/// prompt's body AND records the reference; with neither, both columns are
/// cleared and the session reverts to the standing Peckboard prompt.
pub(crate) fn set_session_system_prompt_impl(
    db: &Db,
    input: &str,
    inv: &InvocationContext,
) -> String {
    // Cap to a sane size so a runaway prompt can't bloat a session row.
    const MAX_LEN: usize = 100_000;

    let req: SetSessionSystemPromptRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    let (prompt, prompt_name) = if let Some(raw) = req.system_prompt {
        (Some(raw), None)
    } else if let Some(name) = req.name {
        match db.get_system_prompt_by_name_blocking(&name) {
            Ok(Some(entry)) => (Some(entry.body), Some(entry.name)),
            Ok(None) => return error_json(format!("no system prompt named '{name}'")),
            Err(e) => return error_json(e),
        }
    } else {
        (None, None)
    };
    if let Some(ref p) = prompt
        && p.len() > MAX_LEN
    {
        return error_json(format!(
            "system_prompt too long ({} > {MAX_LEN} chars)",
            p.len()
        ));
    }
    // Authorize against the current row before writing.
    if let Err(e) = fetch_visible_session(db, req.session_id.trim(), inv) {
        return error_json(e);
    }
    let was_set = prompt.is_some();
    match db.set_session_system_prompt_blocking(req.session_id.trim(), prompt, prompt_name) {
        Ok(Some(session)) => serde_json::json!({
            "session_id": session.id,
            "session_name": session.name,
            "system_prompt_set": was_set,
        })
        .to_string(),
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
// never a plugin-supplied id. Containment (relative-only paths, canonicalized
// re-check, lstat walks, depth/size caps) lives in
// [`crate::service::fs_jail`], shared with core's own folder-scoped routes so
// a plugin's view and the app's view of a folder can't drift apart.

#[derive(Deserialize)]
struct ReadFileRequest {
    path: String,
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

/// `peckboard_list_project_files` — list files (relative path + byte size)
/// under the caller's folder, for size-balanced scope partitioning. Paths are
/// relative to the folder root; `truncated` is `true` if the file cap was hit.
pub(crate) fn list_project_files_impl(db: &Db, inv: &InvocationContext) -> String {
    let root = match caller_folder_root(db, inv) {
        Ok(r) => r,
        Err(e) => return error_json(e),
    };
    let (walked, truncated) = fs_jail::walk_files(&root, &|_| true);
    let files: Vec<serde_json::Value> = walked
        .into_iter()
        .map(|f| serde_json::json!({ "path": f.path, "size": f.size }))
        .collect();
    serde_json::json!({ "files": files, "truncated": truncated }).to_string()
}

/// `peckboard_read_file` — read one UTF-8 text file under the caller's folder.
/// The path must be relative and stay within the folder; content is capped at
/// [`fs_jail::MAX_READ_BYTES`] (`truncated` flags a clipped read).
pub(crate) fn read_file_impl(db: &Db, input: &str, inv: &InvocationContext) -> String {
    let req: ReadFileRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    let root = match caller_folder_root(db, inv) {
        Ok(r) => r,
        Err(e) => return error_json(e),
    };
    let canon = match fs_jail::resolve_read(&root, Path::new(&req.path)) {
        Ok(p) => p,
        Err(e) => return error_json(e),
    };
    let size = std::fs::metadata(&canon).map(|m| m.len()).unwrap_or(0);
    let bytes = match std::fs::read(&canon) {
        Ok(b) => b,
        Err(e) => return error_json(e),
    };
    let truncated = bytes.len() > fs_jail::MAX_READ_BYTES;
    let slice = &bytes[..bytes.len().min(fs_jail::MAX_READ_BYTES)];
    // Lossy so a clip at a multi-byte boundary (or a stray non-UTF-8 byte in an
    // otherwise-text file) still returns usable content rather than erroring.
    let content = String::from_utf8_lossy(slice).into_owned();
    serde_json::json!({
        "content": content,
        "truncated": truncated,
        "size": size,
    })
    .to_string()
}

/// `peckboard_read_file_base64` — read one file under the caller's folder and
/// return its **raw bytes** base64-encoded, so binary content (images, etc.)
/// survives intact rather than being mangled by the lossy UTF-8 decode
/// [`fs_jail::MAX_READ_BYTES`] cap (`truncated` flags a clipped read).
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
    let canon = match fs_jail::resolve_read(&root, Path::new(&req.path)) {
        Ok(p) => p,
        Err(e) => return error_json(e),
    };
    let size = std::fs::metadata(&canon).map(|m| m.len()).unwrap_or(0);
    let bytes = match std::fs::read(&canon) {
        Ok(b) => b,
        Err(e) => return error_json(e),
    };
    let truncated = bytes.len() > fs_jail::MAX_READ_BYTES;
    let slice = &bytes[..bytes.len().min(fs_jail::MAX_READ_BYTES)];
    let base64 = base64::engine::general_purpose::STANDARD.encode(slice);
    serde_json::json!({
        "base64": base64,
        "truncated": truncated,
        "size": size,
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
    let final_path = match fs_jail::resolve_write(&root, Path::new(&req.path), req.create_dirs) {
        Ok(p) => p,
        Err(e) => return error_json(e),
    };

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
/// the caller's scope (e.g. an expert reading its slice). Takes the
/// [`TrustedCaller`] proof: reachable only from a tool invocation or an
/// authenticated `user_authority` plugin-UI request.
pub(crate) fn dispatch_capture_impl(
    db: &Db,
    input: &str,
    caller: &TrustedCaller,
    live: Option<Arc<dyn LiveHost>>,
) -> String {
    let req: DispatchCaptureRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    if let Err(e) = fetch_visible_session(db, req.session_id.trim(), caller) {
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
// Control another session by id. Same-folder targets are free; cross-folder
// targets require an Always / Once grant in the plugin document store (see
// `session_control_auth`). Every action is fire-and-forget via the LiveHost.

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

/// Look up a session by id with NO visibility boundary (discovery + grant
/// checks decide whether the caller may act). Returns a uniform "not found"
/// so a bad id is a clean error rather than a panic.
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

/// `peckboard_list_sessions_brief` — slim, read-only enumeration of every
/// session for visualization plugins (e.g. chicken-coop's one-bird-per-
/// session roster): kind flags and lineage only. Deliberately omits
/// conversation ids, models, folder ids, and prompt content; gated on
/// `session_read` like `peckboard_session_events`.
pub(crate) fn list_sessions_brief_impl(db: &Db) -> String {
    let sessions = match db.list_sessions_blocking() {
        Ok(s) => s,
        Err(e) => return error_json(e.to_string()),
    };
    let out: Vec<serde_json::Value> = sessions
        .into_iter()
        .map(|s| {
            serde_json::json!({
                "session_id": s.id,
                "name": s.name,
                "is_worker": s.is_worker,
                "is_expert": s.is_expert,
                "expert_kind": s.expert_kind,
                "card_id": s.card_id,
                "project_id": s.project_id,
                "parent_session_id": s.parent_session_id,
                "is_temp": s.is_temp,
                "repeating_task_id": s.repeating_task_id,
                "last_activity": s.last_activity,
                "subagent_completed_at": s.subagent_completed_at,
            })
        })
        .collect();
    serde_json::json!({ "sessions": out }).to_string()
}

fn control_session(
    db: &Db,
    plugin_id: &str,
    inv: &InvocationContext,
    input: &str,
    live: Option<Arc<dyn LiveHost>>,
    action_name: &str,
    action: impl FnOnce(Arc<dyn LiveHost>, String),
) -> String {
    let req: SessionControlRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    let target = match require_session(db, &req.session_id) {
        Ok(s) => s,
        Err(e) => return error_json(e),
    };
    if let Err(e) = crate::plugin::session_control_auth::authorize(db, plugin_id, inv, &target) {
        return error_json(e);
    }
    let sid = target.id;
    let Some(live) = live else {
        return error_json("live control unavailable");
    };
    action(live, sid.clone());
    serde_json::json!({ "ok": true, "session_id": sid, "action": action_name }).to_string()
}

pub(crate) fn interrupt_session_impl(
    db: &Db,
    plugin_id: &str,
    inv: &InvocationContext,
    input: &str,
    live: Option<Arc<dyn LiveHost>>,
) -> String {
    control_session(db, plugin_id, inv, input, live, "interrupt", |live, sid| {
        live.interrupt_session(sid)
    })
}

pub(crate) fn terminate_agent_impl(
    db: &Db,
    plugin_id: &str,
    inv: &InvocationContext,
    input: &str,
    live: Option<Arc<dyn LiveHost>>,
) -> String {
    control_session(db, plugin_id, inv, input, live, "terminate", |live, sid| {
        live.terminate_agent(sid)
    })
}

pub(crate) fn clear_session_impl(
    db: &Db,
    plugin_id: &str,
    inv: &InvocationContext,
    input: &str,
    live: Option<Arc<dyn LiveHost>>,
) -> String {
    control_session(db, plugin_id, inv, input, live, "clear", |live, sid| {
        live.clear_session(sid)
    })
}

/// `peckboard_send_message` — deliver a message (with optional base64 image /
/// file attachments) to a session and resume it (same-folder free; cross-
/// folder needs Always/Once — see `session_control_auth`).
pub(crate) fn send_message_impl(
    db: &Db,
    plugin_id: &str,
    inv: &InvocationContext,
    input: &str,
    live: Option<Arc<dyn LiveHost>>,
) -> String {
    use base64::Engine as _;

    let req: SendMessageRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    let target = match require_session(db, &req.session_id) {
        Ok(s) => s,
        Err(e) => return error_json(e),
    };
    if let Err(e) = crate::plugin::session_control_auth::authorize(db, plugin_id, inv, &target) {
        return error_json(e);
    }
    let sid = target.id;
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

// ── Orchestrate: unattended session control (gated, context-free) ────
//
// `session_orchestrate` is a STANDING grant: approving a plugin that
// requests it authorizes these actions with no caller context at all, so
// they work from lifecycle dispatches (`timer.tick`, `session.agent.ended`)
// where the context-checked twins above refuse. Folder-blind by design — an
// orchestrating plugin acts on sessions the user configured, not on a
// caller's behalf — so there is no per-call cross-folder gate; the
// permission itself is the approval surface. Deliberately a separate,
// minimal quartet rather than a relaxation of the existing gates: the
// context-checked functions stay context-checked.

#[derive(Deserialize)]
struct OrchestrateSendRequest {
    session_id: String,
    text: String,
}

pub(crate) fn orchestrate_send_impl(
    db: &Db,
    input: &str,
    live: Option<Arc<dyn LiveHost>>,
) -> String {
    let req: OrchestrateSendRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    if req.text.trim().is_empty() {
        return error_json("orchestrate_send requires non-empty text");
    }
    let target = match require_session(db, &req.session_id) {
        Ok(s) => s,
        Err(e) => return error_json(e),
    };
    let Some(live) = live else {
        return error_json("live control unavailable");
    };
    live.send_message(target.id.clone(), req.text, Vec::new());
    serde_json::json!({ "ok": true, "session_id": target.id }).to_string()
}

#[derive(Deserialize)]
struct OrchestrateCreateSessionRequest {
    folder_id: String,
    name: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    system_prompt: Option<String>,
}

pub(crate) fn orchestrate_create_session_impl(db: &Db, input: &str) -> String {
    let req: OrchestrateCreateSessionRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    if req.name.trim().is_empty() {
        return error_json("name is required");
    }
    // The folder must be named explicitly (no caller scope to inherit) and
    // must exist — a typo'd id would otherwise create an unreachable session.
    match db.get_folder_blocking(req.folder_id.trim()) {
        Ok(Some(_)) => {}
        Ok(None) => return error_json(format!("folder not found: {}", req.folder_id.trim())),
        Err(e) => return error_json(e.to_string()),
    }
    // Mirror create_session_impl's cap so a runaway prompt can't bloat the row.
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
        id: uuid::Uuid::new_v4().to_string(),
        name: req.name,
        folder_id: req.folder_id.trim().to_string(),
        model: req.model,
        effort: req.effort,
        system_prompt: req.system_prompt,
        created_at: now.clone(),
        last_activity: now,
        // No caller session to inherit from: the sole user on single-user
        // installs, else NULL.
        user_id: db.resolve_spawned_session_owner_blocking(None),
        ..Default::default()
    };
    match db.create_session_blocking(new) {
        Ok(session) => serde_json::json!({ "session": session }).to_string(),
        Err(e) => error_json(e),
    }
}

#[derive(Deserialize)]
struct OrchestrateSetPromptRequest {
    session_id: String,
    #[serde(default)]
    system_prompt: Option<String>,
}

pub(crate) fn orchestrate_set_prompt_impl(db: &Db, input: &str) -> String {
    const MAX_LEN: usize = 100_000;
    let req: OrchestrateSetPromptRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    if let Some(ref p) = req.system_prompt
        && p.len() > MAX_LEN
    {
        return error_json(format!(
            "system_prompt too long ({} > {MAX_LEN} chars)",
            p.len()
        ));
    }
    if let Err(e) = require_session(db, &req.session_id) {
        return error_json(e);
    }
    let was_set = req.system_prompt.is_some();
    match db.set_session_system_prompt_blocking(req.session_id.trim(), req.system_prompt, None) {
        Ok(Some(session)) => serde_json::json!({
            "session_id": session.id,
            "session_name": session.name,
            "system_prompt_set": was_set,
        })
        .to_string(),
        Ok(None) => error_json("session not found"),
        Err(e) => error_json(e),
    }
}

#[derive(Deserialize)]
struct OrchestrateSessionStateRequest {
    session_id: String,
}

pub(crate) fn orchestrate_session_state_impl(db: &Db, input: &str) -> String {
    let req: OrchestrateSessionStateRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    let id = req.session_id.trim();
    if id.is_empty() {
        return error_json("session_id is required");
    }
    match db.get_session_blocking(id) {
        Ok(Some(s)) => serde_json::json!({
            "exists": true,
            "session": {
                "id": s.id,
                "name": s.name,
                "folder_id": s.folder_id,
                "model": s.model,
                "is_worker": s.is_worker,
                "last_activity": s.last_activity,
            },
        })
        .to_string(),
        Ok(None) => serde_json::json!({ "exists": false }).to_string(),
        Err(e) => error_json(e.to_string()),
    }
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

/// Scratch working directory (under the data dir) for a folder-less
/// full-authority exec. Never the data dir itself — that is where
/// `ssh_vault_key` / `jwt_secret` / `peckboard.db` live.
const PLUGIN_EXEC_DIR: &str = "plugin-exec";

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
///
/// `authority_root` is the data dir a folder-less full-**user**-authority
/// caller (a plugin's own authenticated UI page — a global `sidebar_items`
/// page has no project/session to resolve a folder from) falls back to. The
/// cwd used is [`PLUGIN_EXEC_DIR`] *underneath* it, never the data dir
/// itself: `<data_dir>` holds `ssh_vault_key`, `jwt_secret`, and
/// `peckboard.db`, and the allowlist admits general-purpose interpreters
/// (`python3`, `node`, `ruby`, …), so a cwd of `<data_dir>` would hand any
/// `process_exec` plugin the SSH key vault by relative path. `None` (the MCP
/// tool bridge, which is never full authority) keeps the refusal: the
/// per-folder floor is what keeps a tool call inside the calling session's
/// reach, and no user stands behind it to widen it.
pub(crate) fn exec_impl(
    db: &Db,
    input: &str,
    inv: &InvocationContext,
    enforce_allowlist: bool,
    authority_root: Option<&std::path::Path>,
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
    let authority_fallback = authority_root
        .filter(|_| inv.authority && inv.folder_id.is_none())
        .map(|p| p.join(PLUGIN_EXEC_DIR));
    let root = match caller_folder_root(db, inv) {
        Ok(r) => r,
        // The exec cwd is a working directory, not a jail (unlike the
        // fs_jail-backed file functions): what bounds this call is the
        // permission grant plus the bare-name check, both already applied.
        // It is still deliberately a scratch dir rather than the data dir —
        // see `authority_root` above.
        Err(_) if authority_fallback.is_some() => {
            let dir = authority_fallback.unwrap();
            if let Err(e) = std::fs::create_dir_all(&dir) {
                return error_json(format!("failed to prepare the exec working directory: {e}"));
            }
            dir
        }
        Err(e) => return error_json(e),
    };
    let timeout = Duration::from_secs(
        req.timeout_secs
            .unwrap_or(EXEC_DEFAULT_TIMEOUT_SECS)
            .clamp(1, EXEC_MAX_TIMEOUT_SECS),
    );

    // Commands run WITH the custom env vars (Settings → Environment
    // Variables) visible to this folder — globals plus the folder's own,
    // folder winning on a name collision; plain always, encrypted while ANY
    // user's unlock cache is warm (a deliberate DB-wide sharing decision —
    // see the doc comment on `command_env_blocking`) — layered over the
    // inherited host env (custom wins on collision: user-configured beats
    // ambient). The agent itself never gets these values: its own process
    // env carries no custom vars, and any secret a command prints is masked
    // below before the agent can read it.
    let (inject_env, masker) =
        crate::service::secret_mask::command_env_blocking(db, inv.folder_id.as_deref());
    use std::process::{Command, Stdio};
    let mut child = match Command::new(command)
        .args(&req.args)
        .envs(inject_env.iter().map(|(k, v)| (k, v)))
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

    // Console output is the one surface where an env secret could reach the
    // agent — mask known secret values (verbatim or interleaved) with `*`.
    let stdout = String::from_utf8_lossy(&stdout);
    let stderr = String::from_utf8_lossy(&stderr);
    serde_json::json!({
        "exit_code": status.and_then(|s| s.code()),
        "stdout": masker.mask(&stdout),
        "stderr": masker.mask(&stderr),
        "stdout_truncated": stdout_truncated,
        "stderr_truncated": stderr_truncated,
        "timed_out": timed_out,
    })
    .to_string()
}

/// Run a JSON envelope's `stdout`/`stderr` fields through the secret masker
/// (custom env values + sensitive-named host env). Non-object or unmatched
/// output passes through unchanged. Blocking — same thread contract as
/// [`exec_impl`].
fn mask_console_envelope(db: &Db, out: String) -> String {
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&out) else {
        return out;
    };
    let masker = crate::service::secret_mask::masker_blocking(db);
    crate::service::secret_mask::mask_console_fields(&mut v, &masker);
    v.to_string()
}

/// Mask every string field of a JSON tool envelope (not just `stdout`/
/// `stderr`) — for read-style host calls (e.g. `peckboard_ssh_read_file`)
/// whose secret-bearing output isn't confined to a console field.
fn mask_full_envelope(db: &Db, out: String) -> String {
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&out) else {
        return out;
    };
    let masker = crate::service::secret_mask::masker_blocking(db);
    crate::service::secret_mask::mask_json_strings(&mut v, &masker);
    v.to_string()
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

/// JSON request for `peckboard_session_questions`.
#[derive(Deserialize)]
struct SessionQuestionsRequest {
    session_id: String,
}

/// `peckboard_session_questions` — the unresolved `question` events of a
/// session visible to the caller, FULL payloads included (question text,
/// options, card context), unlike the deliberately-slim
/// `peckboard_session_events`. Exists so a UI plugin can render a pending
/// question for the user; gated on the dedicated `worker_questions`
/// permission.
pub(crate) fn session_questions_impl(db: &Db, inv: &InvocationContext, input: &str) -> String {
    let req: SessionQuestionsRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    let session = match fetch_visible_session(db, req.session_id.trim(), inv) {
        Ok(s) => s,
        Err(e) => return error_json(e),
    };
    let events = match db.list_events_by_session_blocking(&session.id) {
        Ok(e) => e,
        Err(e) => return error_json(e.to_string()),
    };
    let resolved: std::collections::HashSet<String> = events
        .iter()
        .filter(|e| e.kind == "question-resolved")
        .filter_map(|e| serde_json::from_str::<serde_json::Value>(&e.data).ok())
        .filter_map(|d| {
            d.get("question_id")
                .or_else(|| d.get("questionId"))
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .collect();
    let items: Vec<serde_json::Value> = events
        .iter()
        .filter(|e| e.kind == "question" && !resolved.contains(&e.id))
        .map(|e| {
            serde_json::json!({
                "id": e.id,
                "seq": e.seq,
                "ts": e.ts,
                "data": serde_json::from_str::<serde_json::Value>(&e.data).unwrap_or_default(),
            })
        })
        .collect();
    serde_json::json!({ "questions": items }).to_string()
}

/// JSON request for `peckboard_answer_question`.
#[derive(Deserialize)]
struct AnswerQuestionRequest {
    session_id: String,
    question_id: String,
    #[serde(default)]
    answers: serde_json::Value,
    #[serde(default)]
    rejected: bool,
}

/// `peckboard_answer_question` — resolve a pending question on a session
/// visible to the user, as that user. Validates the question exists in the
/// session and is unresolved, then hands off to the [`LiveHost`], which runs
/// the exact resolution flow of core's own answer route (event + broadcast,
/// expert feed, conversation resume). Requires the trusted [`UserContext`] of
/// an authenticated plugin-UI request: a question is a prompt to the *user*,
/// so only a user-driven surface may answer it — an agent-driven MCP
/// invocation may not.
pub(crate) fn answer_question_impl(
    db: &Db,
    input: &str,
    user: &UserContext,
    live: Option<Arc<dyn LiveHost>>,
) -> String {
    let req: AnswerQuestionRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid request: {e}")),
    };
    let inv = user.as_invocation();
    let session = match fetch_visible_session(db, req.session_id.trim(), &inv) {
        Ok(s) => s,
        Err(e) => return error_json(e),
    };
    let qid = req.question_id.trim().to_string();
    if qid.is_empty() {
        return error_json("question_id is required");
    }
    let events = match db.list_events_by_session_blocking(&session.id) {
        Ok(e) => e,
        Err(e) => return error_json(e.to_string()),
    };
    if !events.iter().any(|e| e.id == qid && e.kind == "question") {
        return error_json("question not found");
    }
    let already = events.iter().any(|e| {
        e.kind == "question-resolved"
            && serde_json::from_str::<serde_json::Value>(&e.data)
                .ok()
                .and_then(|d| {
                    d.get("question_id")
                        .or_else(|| d.get("questionId"))
                        .and_then(|v| v.as_str())
                        .map(String::from)
                })
                .as_deref()
                == Some(qid.as_str())
    });
    if already {
        return error_json("question already resolved");
    }
    let Some(live) = live else {
        return error_json("live host unavailable; answering requires the running app");
    };
    live.answer_question(
        session.id,
        qid,
        req.answers,
        req.rejected,
        user.user_id.clone(),
    );
    serde_json::json!({ "ok": true }).to_string()
}

/// Like [`state_and_permission`] but returns the **trusted** authenticated-
/// user context (`None` outside a plugin-UI request) and the late-bound
/// [`LiveHost`]. For host functions that act strictly under *user* authority
/// — never under an agent's MCP invocation.
#[allow(clippy::type_complexity)]
fn state_permission_user_and_live(
    user_data: &UserData<HostState>,
    permission: &str,
) -> Result<
    (
        Db,
        String,
        bool,
        Option<UserContext>,
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
    let user = state
        .user
        .read()
        .map_err(|_| anyhow::anyhow!("plugin user context poisoned"))?
        .clone();
    let live = state
        .live
        .read()
        .map_err(|_| anyhow::anyhow!("plugin live host poisoned"))?
        .clone();
    Ok((
        state.db.clone(),
        state.plugin_id.clone(),
        granted,
        user,
        live,
    ))
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

/// Like [`state_permission_and_invocation`] but also clones the app data dir.
/// The exec host functions need it as [`exec_impl`]'s `authority_root`: the
/// cwd for a full-authority UI caller that carried no folder scope.
#[allow(clippy::type_complexity)]
fn state_permission_invocation_and_data_dir(
    user_data: &UserData<HostState>,
    permission: &str,
) -> Result<(Db, PathBuf, String, bool, Option<InvocationContext>), Error> {
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
        state.data_dir.clone(),
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

/// Build the [`TrustedCaller`] proof for the current call, if the plugin is
/// in a trusted caller context: an in-flight MCP invocation's verified scope,
/// else — when the plugin is serving an authenticated user request AND holds
/// the `user_authority` permission — the user's full-authority context.
/// `None` otherwise (init, ordinary hooks, public requests), so session
/// dispatch refuses. Unlike [`effective_context`], the user path here
/// re-checks the `user_authority` grant locally instead of leaning on the
/// manifest rule that authed routes imply it.
fn trusted_caller(
    state: &std::sync::MutexGuard<'_, HostState>,
) -> Result<Option<TrustedCaller>, Error> {
    if let Some(inv) = state
        .invocation
        .read()
        .map_err(|_| anyhow::anyhow!("plugin invocation context poisoned"))?
        .clone()
    {
        return Ok(Some(TrustedCaller(inv)));
    }
    let Some(user) = state
        .user
        .read()
        .map_err(|_| anyhow::anyhow!("plugin user context poisoned"))?
        .clone()
    else {
        return Ok(None);
    };
    let granted = state
        .permissions
        .read()
        .map_err(|_| anyhow::anyhow!("plugin permission set poisoned"))?
        .contains("user_authority");
    Ok(granted.then(|| TrustedCaller(user.as_invocation())))
}

/// Like [`state_permission_and_invocation`] but yields the [`TrustedCaller`]
/// proof token instead of a bare context — for the session-dispatch host
/// functions, which must not be reachable with a context that didn't come
/// from a trusted slot.
fn state_permission_and_trusted_caller(
    user_data: &UserData<HostState>,
    permission: &str,
) -> Result<(Db, String, bool, Option<TrustedCaller>), Error> {
    let state = user_data.get()?;
    let state = state
        .lock()
        .map_err(|_| anyhow::anyhow!("plugin host state mutex poisoned"))?;
    let granted = state
        .permissions
        .read()
        .map_err(|_| anyhow::anyhow!("plugin permission set poisoned"))?
        .contains(permission);
    let caller = trusted_caller(&state)?;
    Ok((state.db.clone(), state.plugin_id.clone(), granted, caller))
}

/// [`state_permission_and_trusted_caller`] plus the late-bound [`LiveHost`].
#[allow(clippy::type_complexity)]
fn state_permission_trusted_caller_and_live(
    user_data: &UserData<HostState>,
    permission: &str,
) -> Result<
    (
        Db,
        String,
        bool,
        Option<TrustedCaller>,
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
    let caller = trusted_caller(&state)?;
    let live = state
        .live
        .read()
        .map_err(|_| anyhow::anyhow!("plugin live host poisoned"))?
        .clone();
    Ok((
        state.db.clone(),
        state.plugin_id.clone(),
        granted,
        caller,
        live,
    ))
}

/// Like [`state_and_permission`] but also resolves the late-bound provider
/// registry (upgraded from its `Weak`): `None` before `main.rs` binds it or
/// when no app is running, so `peckboard_list_models` refuses cleanly.
#[allow(clippy::type_complexity)]
fn state_permission_and_registry(
    user_data: &UserData<HostState>,
    permission: &str,
) -> Result<
    (
        Db,
        String,
        bool,
        Option<Arc<crate::provider::registry::ProviderRegistry>>,
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
    let registry = state
        .provider_registry
        .read()
        .map_err(|_| anyhow::anyhow!("plugin provider registry slot poisoned"))?
        .as_ref()
        .and_then(std::sync::Weak::upgrade);
    Ok((state.db.clone(), state.plugin_id.clone(), granted, registry))
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

host_fn!(peckboard_list_folders(user_data: HostState; _input: String) -> String {
    let (db, _plugin_id) = state_from(&user_data)?;
    Ok(list_folders_impl(&db))
});
// `peckboard_list_models` — the selectable (thinking-only) model catalog,
// metadata only; see `list_models_impl`.
host_fn!(peckboard_list_models(user_data: HostState; _input: String) -> String {
    let (db, _plugin_id, ok, registry) = state_permission_and_registry(&user_data, "models_read")?;
    if !ok { return Ok(error_json("plugin lacks the 'models_read' permission")); }
    Ok(list_models_impl(&db, registry))
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

// `peckboard_caller_scope` — the folder/project/session this call is running
// in, as core already resolved it: the in-flight MCP invocation's verified
// scope, or the scope of the authenticated page request (see
// `PluginManager::resolve_authed_scope`).
//
// Ungated: it tells a plugin only where it already *is*. Without it a plugin
// serving its own UI has no way to name the folder it is acting in — the
// session lookup is invocation-only and ownership-gated — so per-folder state
// could not be keyed at all. `authority` says which of the two it is, so a
// plugin can tell an operator's click from an agent's tool call.
host_fn!(peckboard_caller_scope(user_data: HostState; _input: String) -> String {
    let state = user_data.get()?;
    let state = state
        .lock()
        .map_err(|_| anyhow::anyhow!("plugin host state mutex poisoned"))?;
    let ctx = effective_context(&state)?;
    Ok(match ctx {
        Some(inv) => serde_json::json!({
            "folder_id": inv.folder_id,
            "project_id": inv.project_id,
            "session_id": inv.session_id,
            "authority": inv.authority,
        })
        .to_string(),
        None => serde_json::json!({
            "folder_id": null,
            "project_id": null,
            "session_id": null,
            "authority": false,
        })
        .to_string(),
    })
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

host_fn!(peckboard_session_events(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok) = state_and_permission(&user_data, "session_read")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_read' permission")); }
    Ok(session_events_impl(&db, &input))
});

host_fn!(peckboard_list_sessions_brief(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok) = state_and_permission(&user_data, "session_read")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_read' permission")); }
    let _ = input;
    Ok(list_sessions_brief_impl(&db))
});

host_fn!(peckboard_session_questions(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok, inv) = state_permission_and_invocation(&user_data, "worker_questions")?;
    if !ok { return Ok(error_json("plugin lacks the 'worker_questions' permission")); }
    let Some(inv) = inv else { return Ok(error_json("no caller context; peckboard_session_questions requires an authenticated request or tool invocation")); };
    Ok(session_questions_impl(&db, &inv, &input))
});

host_fn!(peckboard_answer_question(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok, user, live) = state_permission_user_and_live(&user_data, "worker_questions")?;
    if !ok { return Ok(error_json("plugin lacks the 'worker_questions' permission")); }
    let Some(user) = user else { return Ok(error_json("peckboard_answer_question is only callable from an authenticated plugin-UI request")); };
    Ok(answer_question_impl(&db, &input, &user, live))
});
// ── Generic session / event host functions (gated, scoped) ────────────
// Each requires a trusted context: an in-flight `mcp.tool.invoke`, or — for
// the [`TrustedCaller`]-taking functions — an authenticated `user_authority`
// plugin-UI request (`serve_http_authed`).

host_fn!(peckboard_create_session(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok, caller) = state_permission_and_trusted_caller(&user_data, "session_write")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_write' permission")); }
    let Some(caller) = caller else { return Ok(error_json("no trusted caller context; peckboard_create_session requires a tool invocation or an authenticated plugin-UI request")); };
    Ok(create_session_impl(&db, &input, &caller))
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

host_fn!(peckboard_set_session_system_prompt(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok, inv) = state_permission_and_invocation(&user_data, "session_prompt_write")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_prompt_write' permission")); }
    let Some(inv) = inv else { return Ok(error_json("no caller context; peckboard_set_session_system_prompt is only callable during a tool invocation")); };
    Ok(set_session_system_prompt_impl(&db, &input, &inv))
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
    let (db, _plugin_id, ok, caller, live) = state_permission_trusted_caller_and_live(&user_data, "session_dispatch")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_dispatch' permission")); }
    let Some(caller) = caller else { return Ok(error_json("no trusted caller context; peckboard_dispatch_capture requires a tool invocation or an authenticated plugin-UI request")); };
    Ok(dispatch_capture_impl(&db, &input, &caller, live))
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

// Session control: same-folder free; cross-folder needs Always/Once grant.
// Gated on `session_control`; caller context is required for the folder check.
host_fn!(peckboard_interrupt_session(user_data: HostState; input: String) -> String {
    let (db, plugin_id, ok, inv, live) = state_permission_invocation_and_live(&user_data, "session_control")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_control' permission")); }
    let Some(inv) = inv else { return Ok(error_json("no caller context; peckboard_interrupt_session is only callable during a tool invocation")); };
    Ok(interrupt_session_impl(&db, &plugin_id, &inv, &input, live))
});

host_fn!(peckboard_terminate_agent(user_data: HostState; input: String) -> String {
    let (db, plugin_id, ok, inv, live) = state_permission_invocation_and_live(&user_data, "session_control")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_control' permission")); }
    let Some(inv) = inv else { return Ok(error_json("no caller context; peckboard_terminate_agent is only callable during a tool invocation")); };
    Ok(terminate_agent_impl(&db, &plugin_id, &inv, &input, live))
});

host_fn!(peckboard_clear_session(user_data: HostState; input: String) -> String {
    let (db, plugin_id, ok, inv, live) = state_permission_invocation_and_live(&user_data, "session_control")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_control' permission")); }
    let Some(inv) = inv else { return Ok(error_json("no caller context; peckboard_clear_session is only callable during a tool invocation")); };
    Ok(clear_session_impl(&db, &plugin_id, &inv, &input, live))
});

host_fn!(peckboard_send_message(user_data: HostState; input: String) -> String {
    let (db, plugin_id, ok, inv, live) = state_permission_invocation_and_live(&user_data, "session_control")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_control' permission")); }
    let Some(inv) = inv else { return Ok(error_json("no caller context; peckboard_send_message is only callable during a tool invocation")); };
    Ok(send_message_impl(&db, &plugin_id, &inv, &input, live))
});

host_fn!(peckboard_list_all_sessions(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok) = state_and_permission(&user_data, "session_control")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_control' permission")); }
    Ok(list_all_sessions_impl(&db, &input))
});

// The orchestrate quartet — see the impls' section comment: standing grant,
// context-free on purpose, so lifecycle dispatches (timer.tick,
// session.agent.ended) can act. Only the permission gates them.
host_fn!(peckboard_orchestrate_send(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok, _inv, live) = state_permission_invocation_and_live(&user_data, "session_orchestrate")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_orchestrate' permission")); }
    Ok(orchestrate_send_impl(&db, &input, live))
});

host_fn!(peckboard_orchestrate_create_session(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok) = state_and_permission(&user_data, "session_orchestrate")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_orchestrate' permission")); }
    Ok(orchestrate_create_session_impl(&db, &input))
});

host_fn!(peckboard_orchestrate_set_prompt(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok) = state_and_permission(&user_data, "session_orchestrate")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_orchestrate' permission")); }
    Ok(orchestrate_set_prompt_impl(&db, &input))
});

host_fn!(peckboard_orchestrate_session_state(user_data: HostState; input: String) -> String {
    let (db, _plugin_id, ok) = state_and_permission(&user_data, "session_orchestrate")?;
    if !ok { return Ok(error_json("plugin lacks the 'session_orchestrate' permission")); }
    Ok(orchestrate_session_state_impl(&db, &input))
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
    let (db, data_dir, _plugin_id, ok, inv) = state_permission_invocation_and_data_dir(&user_data, "process_exec")?;
    if !ok { return Ok(error_json("plugin lacks the 'process_exec' permission")); }
    let Some(inv) = inv else { return Ok(error_json("no caller context; peckboard_exec is only callable during a tool invocation")); };
    Ok(exec_impl(&db, &input, &inv, true, Some(&data_dir)))
});

host_fn!(peckboard_exec_any(user_data: HostState; input: String) -> String {
    let (db, data_dir, _plugin_id, ok, inv) = state_permission_invocation_and_data_dir(&user_data, "process_exec_any")?;
    if !ok { return Ok(error_json("plugin lacks the 'process_exec_any' permission")); }
    let Some(inv) = inv else { return Ok(error_json("no caller context; peckboard_exec_any is only callable during a tool invocation")); };
    Ok(exec_impl(&db, &input, &inv, false, Some(&data_dir)))
});

/// View returned by `peckboard_ssh_key_list` — vault key METADATA only.
/// Deliberately hand-built (never derives from `SshKey` directly, which
/// carries `private_key_ciphertext`/`private_key_nonce`/
/// `passphrase_ciphertext`/`passphrase_nonce`) so a serialization mistake
/// can't leak sealed key material to a plugin.
#[derive(Serialize)]
struct SshKeyListItem {
    id: String,
    name: String,
    key_type: String,
    fingerprint: String,
    has_passphrase: bool,
    created_at: String,
}

impl From<&crate::db::models::SshKey> for SshKeyListItem {
    fn from(k: &crate::db::models::SshKey) -> Self {
        SshKeyListItem {
            id: k.id.clone(),
            name: k.name.clone(),
            key_type: k.key_type.clone(),
            fingerprint: k.fingerprint.clone(),
            has_passphrase: k.passphrase_ciphertext.is_some(),
            created_at: k.created_at.clone(),
        }
    }
}

/// `peckboard_ssh_key_list` backend — list vault key metadata (never the
/// private key, its ciphertext/nonce, or the passphrase).
pub(crate) fn ssh_key_list_impl(db: &Db) -> String {
    match db.list_ssh_keys_blocking() {
        Ok(keys) => {
            let items: Vec<SshKeyListItem> = keys.iter().map(SshKeyListItem::from).collect();
            serde_json::json!({ "keys": items }).to_string()
        }
        Err(e) => error_json(e),
    }
}

/// Shared accessor for the `peckboard_ssh_*` host functions: the `ssh` and
/// `ssh_keys` grants plus enough to resolve a `KeyRef` (`db`, and the data
/// dir the vault key file lives under — see
/// `service::ssh_keys::load_or_create_vault_key`).
fn state_ssh_context(user_data: &UserData<HostState>) -> Result<(Db, bool, bool, PathBuf), Error> {
    let state = user_data.get()?;
    let state = state
        .lock()
        .map_err(|_| anyhow::anyhow!("plugin host state mutex poisoned"))?;
    let permissions = state
        .permissions
        .read()
        .map_err(|_| anyhow::anyhow!("plugin permission set poisoned"))?;
    let has_ssh = permissions.contains("ssh");
    let has_ssh_keys = permissions.contains("ssh_keys");
    Ok((
        state.db.clone(),
        has_ssh,
        has_ssh_keys,
        state.data_dir.clone(),
    ))
}

host_fn!(peckboard_ssh_key_list(user_data: HostState; _input: String) -> String {
    let (db, _has_ssh, has_ssh_keys, _data_dir) = state_ssh_context(&user_data)?;
    if !has_ssh_keys { return Ok(error_json("plugin lacks the 'ssh_keys' permission")); }
    Ok(ssh_key_list_impl(&db))
});

host_fn!(peckboard_ssh_probe(user_data: HostState; input: String) -> String {
    let (db, has_ssh, has_ssh_keys, data_dir) = state_ssh_context(&user_data)?;
    if !has_ssh { return Ok(error_json("plugin lacks the 'ssh' permission")); }
    Ok(super::ssh::probe_impl(&db, &data_dir, has_ssh_keys, &input))
});

host_fn!(peckboard_ssh_exec(user_data: HostState; input: String) -> String {
    let (db, has_ssh, has_ssh_keys, data_dir) = state_ssh_context(&user_data)?;
    if !has_ssh { return Ok(error_json("plugin lacks the 'ssh' permission")); }
    // Remote console output gets the same secret masking as local exec — a
    // remote command can echo back a secret it was handed.
    let out = super::ssh::exec_impl(&db, &data_dir, has_ssh_keys, &input);
    Ok(mask_console_envelope(&db, out))
});

host_fn!(peckboard_ssh_read_file(user_data: HostState; input: String) -> String {
    let (db, has_ssh, has_ssh_keys, data_dir) = state_ssh_context(&user_data)?;
    if !has_ssh { return Ok(error_json("plugin lacks the 'ssh' permission")); }
    // A remote file can contain a secret value verbatim (e.g. the target
    // wrote out its own env) — mask it the same as any other tool output.
    let out = super::ssh::read_file_impl(&db, &data_dir, has_ssh_keys, &input);
    Ok(mask_full_envelope(&db, out))
});

host_fn!(peckboard_ssh_write_file(user_data: HostState; input: String) -> String {
    let (db, has_ssh, has_ssh_keys, data_dir) = state_ssh_context(&user_data)?;
    if !has_ssh { return Ok(error_json("plugin lacks the 'ssh' permission")); }
    Ok(super::ssh::write_file_impl(&db, &data_dir, has_ssh_keys, &input))
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

// ── Plugin-provider host functions (gated by `register_provider`) ─────

/// Shared accessor for the plugin-provider host functions: permission check
/// plus the provider runtime and this plugin's registration staging slot.
#[allow(clippy::type_complexity)]
fn state_permission_and_provider(
    user_data: &UserData<HostState>,
    permission: &str,
) -> Result<
    (
        String,
        bool,
        Arc<crate::provider::plugin_provider::PluginProviderRuntime>,
        Arc<std::sync::RwLock<Option<crate::provider::plugin_provider::ProviderRegistration>>>,
    ),
    Error,
> {
    let state = user_data.get()?;
    let state = state
        .lock()
        .map_err(|_| anyhow::anyhow!("plugin host state mutex poisoned"))?;
    let ok = state
        .permissions
        .read()
        .map(|p| p.contains(permission))
        .unwrap_or(false);
    Ok((
        state.plugin_id.clone(),
        ok,
        state.provider_runtime.clone(),
        state.pending_provider.clone(),
    ))
}

/// Backend for `peckboard_register_provider`: shape-validate and stage the
/// registration for the manager to apply after the `provider.register`
/// dispatch returns. Collision checks against the live registry happen at
/// apply time (`PluginManager::sync_plugin_providers`).
pub(crate) fn register_provider_impl(
    pending: &std::sync::RwLock<Option<crate::provider::plugin_provider::ProviderRegistration>>,
    input: &str,
) -> String {
    let reg: crate::provider::plugin_provider::ProviderRegistration =
        match serde_json::from_str(input) {
            Ok(r) => r,
            Err(e) => return error_json(format!("invalid provider registration: {e}")),
        };
    if let Err(e) = crate::provider::plugin_provider::validate_registration(&reg) {
        return error_json(e);
    }
    let Ok(mut slot) = pending.write() else {
        return error_json("provider registration slot poisoned");
    };
    *slot = Some(reg);
    serde_json::json!({ "ok": true }).to_string()
}

host_fn!(peckboard_register_provider(user_data: HostState; input: String) -> String {
    let (_plugin_id, ok, _runtime, pending) = state_permission_and_provider(&user_data, "register_provider")?;
    if !ok { return Ok(error_json("plugin lacks the 'register_provider' permission")); }
    Ok(register_provider_impl(&pending, &input))
});

host_fn!(peckboard_emit_provider_event(user_data: HostState; input: String) -> String {
    let (plugin_id, ok, runtime, _pending) = state_permission_and_provider(&user_data, "register_provider")?;
    if !ok { return Ok(error_json("plugin lacks the 'register_provider' permission")); }
    Ok(runtime.emit_from_plugin(&plugin_id, &input))
});

host_fn!(peckboard_provider_should_stop(user_data: HostState; input: String) -> String {
    let (plugin_id, ok, runtime, _pending) = state_permission_and_provider(&user_data, "register_provider")?;
    if !ok { return Ok(error_json("plugin lacks the 'register_provider' permission")); }
    Ok(runtime.should_stop_json(&plugin_id, &input))
});

host_fn!(peckboard_provider_take_message(user_data: HostState; input: String) -> String {
    let (plugin_id, ok, runtime, _pending) = state_permission_and_provider(&user_data, "register_provider")?;
    if !ok { return Ok(error_json("plugin lacks the 'register_provider' permission")); }
    Ok(runtime.take_message_json(&plugin_id, &input))
});

host_fn!(peckboard_provider_get_session(user_data: HostState; input: String) -> String {
    let (plugin_id, ok, runtime, _pending) = state_permission_and_provider(&user_data, "register_provider")?;
    if !ok { return Ok(error_json("plugin lacks the 'register_provider' permission")); }
    Ok(runtime.get_session_json(&plugin_id, &input))
});

host_fn!(peckboard_provider_get_mcp_config(user_data: HostState; input: String) -> String {
    let (plugin_id, ok, runtime, _pending) = state_permission_and_provider(&user_data, "register_provider")?;
    if !ok { return Ok(error_json("plugin lacks the 'register_provider' permission")); }
    Ok(runtime.get_mcp_config_json(&plugin_id, &input))
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

// `peckboard_browser_runs` — list recorded browser test runs (newest first)
// as bounded summaries: slim steps + precomputed request/error counts, no
// network/console/pointer payloads (full metas OOM the wasm instance once
// enough runs accumulate — the player fetches one full run via
// `peckboard_browser_run`). Gated by `browser_runs_read`.
host_fn!(peckboard_browser_runs(user_data: HostState; _input: String) -> String {
    let (ok, data_dir) = state_permission_and_data_dir(&user_data, "browser_runs_read")?;
    if !ok { return Ok(error_json("plugin lacks the 'browser_runs_read' permission")); }
    let runs = crate::service::browser_runs::list_run_summaries(&data_dir);
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
    provider_runtime: Arc<crate::provider::plugin_provider::PluginProviderRuntime>,
    pending_provider: Arc<
        std::sync::RwLock<Option<crate::provider::plugin_provider::ProviderRegistration>>,
    >,
    provider_registry: ProviderRegistrySlot,
) -> Vec<Function> {
    let ud = UserData::new(HostState {
        db: db.clone(),
        data_dir,
        plugin_id: plugin_id.to_string(),
        permissions,
        invocation,
        live,
        user,
        provider_runtime,
        pending_provider,
        provider_registry,
    });
    vec![
        Function::new(
            "peckboard_register_provider",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_register_provider,
        ),
        Function::new(
            "peckboard_emit_provider_event",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_emit_provider_event,
        ),
        Function::new(
            "peckboard_provider_should_stop",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_provider_should_stop,
        ),
        Function::new(
            "peckboard_provider_take_message",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_provider_take_message,
        ),
        Function::new(
            "peckboard_provider_get_session",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_provider_get_session,
        ),
        Function::new(
            "peckboard_provider_get_mcp_config",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_provider_get_mcp_config,
        ),
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
            "peckboard_list_folders",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_list_folders,
        ),
        Function::new(
            "peckboard_list_models",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_list_models,
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
            "peckboard_caller_scope",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_caller_scope,
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
            "peckboard_session_events",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_session_events,
        ),
        Function::new(
            "peckboard_list_sessions_brief",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_list_sessions_brief,
        ),
        Function::new(
            "peckboard_session_questions",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_session_questions,
        ),
        Function::new(
            "peckboard_answer_question",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_answer_question,
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
            "peckboard_set_session_system_prompt",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_set_session_system_prompt,
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
            "peckboard_orchestrate_send",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_orchestrate_send,
        ),
        Function::new(
            "peckboard_orchestrate_create_session",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_orchestrate_create_session,
        ),
        Function::new(
            "peckboard_orchestrate_set_prompt",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_orchestrate_set_prompt,
        ),
        Function::new(
            "peckboard_orchestrate_session_state",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_orchestrate_session_state,
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
            "peckboard_ssh_probe",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_ssh_probe,
        ),
        Function::new(
            "peckboard_ssh_exec",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_ssh_exec,
        ),
        Function::new(
            "peckboard_ssh_read_file",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_ssh_read_file,
        ),
        Function::new(
            "peckboard_ssh_write_file",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_ssh_write_file,
        ),
        Function::new(
            "peckboard_ssh_key_list",
            [PTR],
            [PTR],
            ud.clone(),
            peckboard_ssh_key_list,
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

    #[tokio::test]
    async fn list_sessions_brief_impl_maps_kind_fields() {
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
        let mk = |id: &str| crate::db::models::NewSession {
            id: id.into(),
            name: id.into(),
            folder_id: "f1".into(),
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        };
        db.create_session(crate::db::models::NewSession {
            is_worker: true,
            ..mk("worker")
        })
        .await
        .unwrap();
        db.create_session(crate::db::models::NewSession {
            is_expert: true,
            expert_kind: Some("subagent".into()),
            parent_session_id: Some("worker".into()),
            ..mk("chick")
        })
        .await
        .unwrap();
        db.create_session(crate::db::models::NewSession {
            is_temp: true,
            ..mk("temp")
        })
        .await
        .unwrap();

        let out = list_sessions_brief_impl(&db);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let rows = v["sessions"].as_array().unwrap();
        assert_eq!(rows.len(), 3, "all sessions listed: {out}");
        let by_id = |id: &str| {
            rows.iter()
                .find(|r| r["session_id"] == id)
                .unwrap_or_else(|| panic!("missing {id}: {out}"))
        };
        assert_eq!(by_id("worker")["is_worker"], true);
        assert!(by_id("worker")["card_id"].is_null());
        assert_eq!(by_id("chick")["expert_kind"], "subagent");
        assert_eq!(by_id("chick")["parent_session_id"], "worker");
        assert!(by_id("chick")["subagent_completed_at"].is_null());
        assert_eq!(by_id("temp")["is_temp"], true);
        // Key names present even when null, so the plugin can rely on them.
        assert!(out.contains("\"repeating_task_id\""), "key missing: {out}");
        // Slim: no conversation ids, models, or prompt content.
        assert!(!out.contains("conversation_id"), "payload not slim: {out}");
        assert!(!out.contains("system_prompt"), "payload not slim: {out}");
        assert!(!out.contains("\"model\""), "payload not slim: {out}");
    }

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
    async fn session_events_impl_returns_slim_tail() {
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

        db.append_event("s1", "user", serde_json::json!({ "text": "hi" }))
            .await
            .unwrap();
        db.append_event(
            "s1",
            "agent-tool-start",
            serde_json::json!({ "name": "Bash" }),
        )
        .await
        .unwrap();

        // From the beginning: slim shape, tool name surfaced, no `data` leak.
        let out = session_events_impl(&db, r#"{"session_id":"s1"}"#);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let events = v["events"].as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["seq"], 1);
        assert_eq!(events[0]["kind"], "user");
        assert!(
            events[0]["name"].is_null(),
            "non-tool event → null name: {out}"
        );
        assert_eq!(events[1]["kind"], "agent-tool-start");
        assert_eq!(events[1]["name"], "Bash");
        assert_eq!(v["latest_seq"], 2);
        assert!(!out.contains("\"text\""), "event data must not leak: {out}");

        // after_seq skips consumed events; empty tail → null latest_seq.
        let out = session_events_impl(&db, r#"{"session_id":"s1","after_seq":2}"#);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["events"].as_array().unwrap().len(), 0);
        assert!(
            v["latest_seq"].is_null(),
            "no events → null latest_seq: {out}"
        );

        // limit clamps to the window (oldest-first).
        let out = session_events_impl(&db, r#"{"session_id":"s1","limit":1}"#);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["events"].as_array().unwrap().len(), 1);
        assert_eq!(v["events"][0]["seq"], 1);
        assert_eq!(v["latest_seq"], 1);

        // Malformed JSON is an error, not a panic.
        assert!(session_events_impl(&db, "not json").contains("invalid request"));
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
        db.create_folder(NewFolder {
            id: "f2".into(),
            name: "f2".into(),
            path: "/tmp/f2".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_session(crate::db::models::NewSession {
            id: "caller".into(),
            name: "caller".into(),
            folder_id: "f1".into(),
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
        db.create_session(crate::db::models::NewSession {
            id: "s1".into(),
            name: "s1".into(),
            folder_id: "f1".into(),
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
        db.create_session(crate::db::models::NewSession {
            id: "other".into(),
            name: "other".into(),
            folder_id: "f2".into(),
            created_at: ts.clone(),
            last_activity: ts,
            ..Default::default()
        })
        .await
        .unwrap();

        let inv = InvocationContext {
            session_id: Some("caller".into()),
            project_id: None,
            folder_id: Some("f1".into()),
            authority: false,
        };
        let pid = "session-control";

        // Unknown target id → "not found" (no boundary check, just existence).
        let nf = interrupt_session_impl(&db, pid, &inv, r#"{"session_id":"nope"}"#, None);
        assert!(nf.contains("not found"), "{nf}");

        // Same-folder known session, but no live host → unavailable.
        let nl = clear_session_impl(&db, pid, &inv, r#"{"session_id":"s1"}"#, None);
        assert!(nl.contains("live control unavailable"), "{nl}");

        // Cross-folder without grant is refused before live dispatch.
        let xf = interrupt_session_impl(&db, pid, &inv, r#"{"session_id":"other"}"#, None);
        assert!(xf.contains("user approval"), "{xf}");

        // Always grant unlocks cross-folder (still needs live host).
        crate::plugin::session_control_auth::grant_always(&db, pid, "caller").unwrap();
        let nl2 = interrupt_session_impl(&db, pid, &inv, r#"{"session_id":"other"}"#, None);
        assert!(nl2.contains("live control unavailable"), "{nl2}");

        // send_message refuses an empty payload (no text, no attachments).
        let empty = send_message_impl(&db, pid, &inv, r#"{"session_id":"s1","text":"  "}"#, None);
        assert!(empty.contains("requires"), "{empty}");

        // Malformed base64 attachment is rejected before dispatch.
        let bad = send_message_impl(
            &db,
            pid,
            &inv,
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

        let caller = TrustedCaller(InvocationContext {
            session_id: Some("caller".into()),
            project_id: None,
            folder_id: Some("f1".into()),
            authority: false,
        });
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

    #[test]
    fn expand_home_path_rules() {
        let home = Some(std::path::PathBuf::from("/home/u"));
        assert_eq!(
            expand_home_path("~/a/b", home.clone()).unwrap(),
            std::path::PathBuf::from("/home/u/a/b")
        );
        assert_eq!(
            expand_home_path("~", home.clone()).unwrap(),
            std::path::PathBuf::from("/home/u")
        );
        assert_eq!(
            expand_home_path("/abs/x", home.clone()).unwrap(),
            std::path::PathBuf::from("/abs/x")
        );
        assert!(expand_home_path("relative/x", home).is_err());
        assert!(expand_home_path("~/a", None).is_err());
    }

    /// `folder_path` (authority-only) registers the folder on first use,
    /// reuses it afterwards, and `is_temp` lands on the session row.
    #[tokio::test]
    async fn create_session_impl_folder_path_and_is_temp() {
        let db = Db::in_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let installs = tmp.path().join("installs");
        let req = |name: &str| {
            serde_json::json!({
                "name": name,
                "model": "mock:plan-review",
                "is_temp": true,
                "folder_path": installs.to_string_lossy(),
                "folder_name": "App installs",
            })
            .to_string()
        };

        // A tool invocation (no authority) must NOT get to pick a folder.
        let denied = create_session_impl(
            &db,
            &req("Install git"),
            &TrustedCaller(inv(None, Some("f-caller"))),
        );
        assert!(
            denied.contains("authenticated user request"),
            "non-authority folder_path must be refused: {denied}"
        );

        // An authenticated plugin-UI request registers the folder + temp session.
        let caller = TrustedCaller(inv_user());
        let out = create_session_impl(&db, &req("Install git"), &caller);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let sid = v["session"]["id"].as_str().expect("session id").to_string();
        let session = db.get_session(&sid).await.unwrap().unwrap();
        assert!(session.is_temp, "is_temp must persist");
        assert_eq!(session.model.as_deref(), Some("mock:plan-review"));
        assert!(installs.is_dir(), "directory must be created on disk");
        let folders = db.list_folders().await.unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].name, "App installs");
        assert_eq!(folders[0].id, session.folder_id);

        // Second create at the same path reuses the registered folder row.
        let out2 = create_session_impl(&db, &req("Install ripgrep"), &caller);
        let v2: serde_json::Value = serde_json::from_str(&out2).unwrap();
        let sid2 = v2["session"]["id"].as_str().unwrap().to_string();
        let session2 = db.get_session(&sid2).await.unwrap().unwrap();
        assert_eq!(session2.folder_id, session.folder_id);
        assert_eq!(db.list_folders().await.unwrap().len(), 1);

        // A relative path is refused before touching the filesystem.
        let bad = create_session_impl(
            &db,
            r#"{"name":"x","folder_path":"relative/path"}"#,
            &caller,
        );
        assert!(bad.contains("absolute"), "{bad}");
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
                &TrustedCaller(inv(Some(proj), Some(fold))),
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
        let caller = TrustedCaller(inv(Some("p1"), Some("f1")));

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
        let foreign = create_session_impl(
            &db,
            r#"{"name":"foreign"}"#,
            &TrustedCaller(inv(Some("p2"), Some("f2"))),
        );
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

    /// `peckboard_set_session_system_prompt` is gated by **visibility only**
    /// (a plugin may steer a session it never marked), so the folder/project
    /// floor is the whole boundary. Also covers library-name resolution, the
    /// clear-both-columns case, and the length cap.
    #[tokio::test]
    async fn set_session_system_prompt_impl_resolves_scopes_and_caps() {
        let db = setup().await; // folder f1 / project p1
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f2".into(),
            name: "Other".into(),
            path: "/tmp/f2sp".into(),
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
            last_accessed_at: ts.clone(),
            budget_usd_cents: None,
            budget_period: None,
            worktree_isolation: false,
        })
        .await
        .unwrap();
        let mk = |id: &str, folder: &str, project: &str| crate::db::models::NewSession {
            id: id.into(),
            name: id.into(),
            folder_id: folder.into(),
            project_id: Some(project.into()),
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        };
        db.create_session(mk("near", "f1", "p1")).await.unwrap();
        db.create_session(mk("far", "f2", "p2")).await.unwrap();
        db.create_system_prompt("reviewer", "Review hard.", None)
            .await
            .unwrap();
        let caller = inv(Some("p1"), Some("f1"));

        // A raw body wins and carries no library reference.
        let out = set_session_system_prompt_impl(
            &db,
            r#"{"session_id":"near","system_prompt":"Be terse."}"#,
            &caller,
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["system_prompt_set"], true, "set: {out}");
        assert_eq!(v["session_id"], "near");
        assert_eq!(v["session_name"], "near");
        let s = db.get_session("near").await.unwrap().unwrap();
        assert_eq!(s.system_prompt.as_deref(), Some("Be terse."));
        assert_eq!(s.system_prompt_name, None);

        // A library name writes the body AND records where it came from.
        let out = set_session_system_prompt_impl(
            &db,
            r#"{"session_id":"near","name":"reviewer"}"#,
            &caller,
        );
        assert!(out.contains("\"system_prompt_set\":true"), "by name: {out}");
        let s = db.get_session("near").await.unwrap().unwrap();
        assert_eq!(s.system_prompt.as_deref(), Some("Review hard."));
        assert_eq!(s.system_prompt_name.as_deref(), Some("reviewer"));

        // An unknown library name is an error, and leaves the row alone.
        let out =
            set_session_system_prompt_impl(&db, r#"{"session_id":"near","name":"nope"}"#, &caller);
        assert!(
            out.contains("no system prompt named 'nope'"),
            "unknown: {out}"
        );
        assert_eq!(
            db.get_session("near")
                .await
                .unwrap()
                .unwrap()
                .system_prompt_name
                .as_deref(),
            Some("reviewer")
        );

        // Neither field clears BOTH columns.
        let out = set_session_system_prompt_impl(&db, r#"{"session_id":"near"}"#, &caller);
        assert!(out.contains("\"system_prompt_set\":false"), "clear: {out}");
        let s = db.get_session("near").await.unwrap().unwrap();
        assert_eq!(s.system_prompt, None);
        assert_eq!(s.system_prompt_name, None);

        // Over the cap → refused, nothing written.
        let huge = "x".repeat(100_001);
        let out = set_session_system_prompt_impl(
            &db,
            &serde_json::json!({ "session_id": "near", "system_prompt": huge }).to_string(),
            &caller,
        );
        assert!(out.contains("too long"), "oversize: {out}");
        assert_eq!(
            db.get_session("near").await.unwrap().unwrap().system_prompt,
            None
        );

        // A session outside the caller's folder AND project is invisible — the
        // same uniform framing the other scoped session host functions use.
        let out = set_session_system_prompt_impl(
            &db,
            r#"{"session_id":"far","system_prompt":"pwn"}"#,
            &caller,
        );
        assert!(out.contains("session not found"), "cross-folder: {out}");
        assert_eq!(
            db.get_session("far").await.unwrap().unwrap().system_prompt,
            None
        );
        // ...as is a session that doesn't exist at all.
        assert!(
            set_session_system_prompt_impl(&db, r#"{"session_id":"ghost"}"#, &caller)
                .contains("session not found")
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
        // Stands in for the app data dir the exec host functions pass as
        // `authority_root`.
        let data_dir = tempfile::tempdir().unwrap();
        let data_root = data_dir.path().to_path_buf();

        // exec_impl is blocking (DB reads + the env-var unlock snapshot's
        // blocking_lock) — run it on a blocking thread, the exec path's real
        // thread contract, same as `exec_sh`. Calling it on the runtime
        // thread panics once another test has bound the process-global
        // unlock registry.
        tokio::task::spawn_blocking(move || {
            // Not on the allowlist → refused before spawning (allowlist enforced).
            let r = exec_impl(
                &db,
                r#"{"command":"rm","args":["-rf","/"]}"#,
                &caller,
                true,
                Some(&data_root),
            );
            assert!(r.contains("not on the allowlist"), "rm: {r}");

            // The unrestricted variant skips the allowlist (but still bare-name +
            // folder-scoped): `rm` is no longer refused on allowlist grounds.
            let r = exec_impl(
                &db,
                r#"{"command":"rm","args":["--version"]}"#,
                &caller,
                false,
                Some(&data_root),
            );
            assert!(
                !r.contains("not on the allowlist"),
                "exec_any allowlist: {r}"
            );

            // A path component (escape attempt) → refused as not-a-bare-name, even
            // for the unrestricted variant.
            let r = exec_impl(
                &db,
                r#"{"command":"../../bin/sh"}"#,
                &caller,
                false,
                Some(&data_root),
            );
            assert!(r.contains("bare executable name"), "path: {r}");

            // No folder scope on an MCP invocation → still refused (the
            // per-folder floor is what keeps a plugin tool inside the calling
            // session's reach).
            let unscoped = inv(Some("pE"), None);
            let r = exec_impl(
                &db,
                r#"{"command":"git","args":["--version"]}"#,
                &unscoped,
                true,
                Some(&data_root),
            );
            assert!(r.contains("folder"), "unscoped: {r}");

            // A full-authority UI caller (a plugin's global sidebar page) has
            // no folder to resolve, so it runs in a scratch dir under the app
            // data dir instead of being refused — the cwd is a working
            // directory here, not a jail. It must NOT be the data dir itself:
            // that is where `ssh_vault_key` and `peckboard.db` live, and the
            // allowlist admits interpreters that can read whatever the cwd
            // contains.
            std::fs::write(data_root.join("ssh_vault_key"), b"pretend-vault-key").unwrap();
            let authority = InvocationContext {
                authority: true,
                ..Default::default()
            };
            let r = exec_impl(
                &db,
                r#"{"command":"pwd"}"#,
                &authority,
                false,
                Some(&data_root),
            );
            let v: serde_json::Value = serde_json::from_str(&r).unwrap();
            assert!(v.get("error").is_none(), "authority exec: {r}");
            let cwd = v["stdout"].as_str().unwrap_or("").trim().to_string();
            assert!(
                cwd.ends_with(PLUGIN_EXEC_DIR),
                "authority cwd must be the scratch dir, got: {cwd}"
            );
            assert!(
                !std::path::Path::new(&cwd).join("ssh_vault_key").exists(),
                "the vault key must not sit in the exec cwd: {cwd}"
            );
            // Allowlisted command runs in the folder when the tool is present.
            let r = exec_impl(
                &db,
                r#"{"command":"git","args":["--version"]}"#,
                &caller,
                true,
                Some(&data_root),
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
        })
        .await
        .unwrap();
    }

    /// A plugin-driven model/effort change must recycle the session's live
    /// agent process — it keeps its spawn-time model and account credentials,
    /// so reusing it would answer (and bill) as the old model/account.
    /// Unrelated updates and no-op writes must not recycle.
    #[tokio::test]
    async fn update_session_model_change_recycles_agent() {
        let db = setup().await; // folder f1 / project p1
        let pid = "experts";
        let caller = TrustedCaller(inv(Some("p1"), Some("f1")));

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
        fn send_message(
            &self,
            session_id: String,
            text: String,
            _attachments: Vec<LiveAttachment>,
        ) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("send:{session_id}:{text}"));
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
        let caller = TrustedCaller(inv(Some("p1"), Some("f1")));

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
            &TrustedCaller(inv(Some("p2"), Some("f2"))),
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

    // ── exec env injection + console secret masking ───────────────────

    /// Db + a real folder (`fx`) the exec tests run in.
    async fn exec_fixture() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::in_memory().unwrap();
        db.create_folder(NewFolder {
            id: "fx".into(),
            name: "fx".into(),
            path: dir.path().to_string_lossy().to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
        })
        .await
        .unwrap();
        (db, dir)
    }

    fn exec_inv() -> InvocationContext {
        InvocationContext {
            folder_id: Some("fx".into()),
            ..Default::default()
        }
    }

    fn custom_var(
        name: &str,
        value: &str,
        folder_id: Option<&str>,
    ) -> crate::db::models::NewEnvVar {
        let ts = chrono::Utc::now().to_rfc3339();
        crate::db::models::NewEnvVar {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            value: Some(value.into()),
            ciphertext: None,
            nonce: None,
            kdf_salt: None,
            encrypted: false,
            encrypted_by: None,
            folder_id: folder_id.map(Into::into),
            created_at: ts.clone(),
            updated_at: ts,
        }
    }

    /// Run `sh -c <script>` through `exec_impl` on a blocking thread (the
    /// exec path's real thread contract) and parse the JSON envelope.
    async fn exec_sh(db: &Db, script: &str) -> serde_json::Value {
        let db = db.clone();
        let script = script.to_string();
        let out = tokio::task::spawn_blocking(move || {
            let req = serde_json::json!({ "command": "sh", "args": ["-c", script] }).to_string();
            // exec_inv() is folder-scoped, so no authority fallback is needed.
            exec_impl(&db, &req, &exec_inv(), false, None)
        })
        .await
        .unwrap();
        serde_json::from_str(&out).unwrap()
    }

    #[tokio::test]
    async fn exec_injects_custom_env_but_masks_its_value_in_output() {
        let (db, _dir) = exec_fixture().await;
        db.upsert_env_var(custom_var("PB_TEST_SECRET", "supersecretvalue123", None))
            .await
            .unwrap();

        // The command really received the value (a length check leaks nothing).
        let v = exec_sh(&db, "echo len=${#PB_TEST_SECRET}").await;
        assert_eq!(v["exit_code"], 0, "envelope: {v}");
        assert!(
            v["stdout"].as_str().unwrap().contains("len=19"),
            "envelope: {v}"
        );

        // Printing it verbatim or with symbols in between is masked.
        let v = exec_sh(
            &db,
            "echo $PB_TEST_SECRET; echo $PB_TEST_SECRET | sed 's/./&-/g'",
        )
        .await;
        let stdout = v["stdout"].as_str().unwrap();
        assert!(!stdout.contains("supersecretvalue123"), "stdout: {stdout}");
        assert!(!stdout.contains("s-u-p-e-r"), "stdout: {stdout}");
        assert!(stdout.contains("********"), "stdout: {stdout}");
    }

    #[tokio::test]
    async fn exec_masks_sensitive_host_env_values() {
        let (db, _dir) = exec_fixture().await;
        // SAFETY: test-only process env mutation; the name is unique to this
        // test and the value appears nowhere else.
        unsafe { std::env::set_var("PB_TEST_HOST_TOKEN", "hostenvsecret987654") };
        let v = exec_sh(&db, "printenv PB_TEST_HOST_TOKEN").await;
        let stdout = v["stdout"].as_str().unwrap();
        assert!(!stdout.contains("hostenvsecret987654"), "stdout: {stdout}");
        assert!(stdout.contains("********"), "stdout: {stdout}");
    }

    // Regression guard: `exec_injects_unlocked_encrypted_vars_and_masks_them`
    // and `exec_injects_another_users_unlocked_value_by_shared_design` each
    // install a fresh global env-unlock registry (`set_global_registry`) and
    // populate it. Run concurrently (the default under `cargo test`), one
    // test's registry replaces the other's mid-flight and either can lose
    // its cached value. Serialize just these two tests against each other.
    static ENV_UNLOCK_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn exec_injects_unlocked_encrypted_vars_and_masks_them() {
        let _guard = ENV_UNLOCK_TEST_LOCK.lock().await;
        let (db, _dir) = exec_fixture().await;
        let ts = chrono::Utc::now().to_rfc3339();
        let row = db
            .upsert_env_var(crate::db::models::NewEnvVar {
                id: uuid::Uuid::new_v4().to_string(),
                name: "PB_TEST_ENC".into(),
                value: None,
                ciphertext: Some("irrelevant".into()),
                nonce: Some("irrelevant".into()),
                kdf_salt: Some("irrelevant".into()),
                encrypted: true,
                encrypted_by: Some("owner-x".into()),
                folder_id: None,
                created_at: ts.clone(),
                updated_at: ts,
            })
            .await
            .unwrap();

        // Seed the unlock cache through the process-global registry (first
        // set wins; use whichever instance is installed).
        crate::service::env_vars::set_global_registry(std::sync::Arc::new(
            crate::service::env_vars::EnvUnlockRegistry::new(),
        ));
        let reg = crate::service::env_vars::global_registry().unwrap();
        let mut vals = std::collections::HashMap::new();
        // The unlock cache is keyed by var id (names are only unique per
        // scope).
        vals.insert(row.id.clone(), "unlockedsecret4321".to_string());
        reg.cache_put("owner-x", vals).await;

        let v = exec_sh(&db, "echo len=${#PB_TEST_ENC}; echo $PB_TEST_ENC").await;
        let stdout = v["stdout"].as_str().unwrap();
        assert!(stdout.contains("len=18"), "stdout: {stdout}");
        assert!(!stdout.contains("unlockedsecret4321"), "stdout: {stdout}");
        assert!(stdout.contains("********"), "stdout: {stdout}");
    }

    #[tokio::test]
    async fn exec_injects_another_users_unlocked_value_by_shared_design() {
        let _guard = ENV_UNLOCK_TEST_LOCK.lock().await;
        // Deliberate design decision (confirmed 2026-08-03, not a bug): an
        // encrypted var's warm-unlocked plaintext is shared DB-wide, not
        // scoped to the user who unlocked it — see the doc comment on
        // `command_env_blocking`. This test documents that intent: a
        // caller with no particular owning session (i.e. not owner-x) still
        // gets owner-x's warm-unlocked value injected, exactly like a plain
        // var. Console output is still masked regardless of who injected it.
        let (db, _dir) = exec_fixture().await;
        let ts = chrono::Utc::now().to_rfc3339();
        let row = db
            .upsert_env_var(crate::db::models::NewEnvVar {
                id: uuid::Uuid::new_v4().to_string(),
                name: "PB_TEST_ENC2".into(),
                value: None,
                ciphertext: Some("irrelevant".into()),
                nonce: Some("irrelevant".into()),
                kdf_salt: Some("irrelevant".into()),
                encrypted: true,
                encrypted_by: Some("owner-x".into()),
                folder_id: None,
                created_at: ts.clone(),
                updated_at: ts,
            })
            .await
            .unwrap();

        crate::service::env_vars::set_global_registry(std::sync::Arc::new(
            crate::service::env_vars::EnvUnlockRegistry::new(),
        ));
        let reg = crate::service::env_vars::global_registry().unwrap();
        let mut vals = std::collections::HashMap::new();
        vals.insert(row.id.clone(), "ownerxsecret999888".to_string());
        // Only owner-x's unlock window is open.
        reg.cache_put("owner-x", vals).await;

        let v = exec_sh(&db, "echo len=${#PB_TEST_ENC2}").await;
        let stdout = v["stdout"].as_str().unwrap();
        assert!(stdout.contains("len=18"), "stdout: {stdout}");

        let v = exec_sh(&db, "echo $PB_TEST_ENC2").await;
        let stdout = v["stdout"].as_str().unwrap();
        assert!(!stdout.contains("ownerxsecret999888"), "stdout: {stdout}");
        assert!(stdout.contains("********"), "stdout: {stdout}");
    }

    #[tokio::test]
    async fn exec_folder_var_shadows_global_and_foreign_folder_masked() {
        let (db, _dir) = exec_fixture().await;
        // Global + same-name var in the caller's folder ("fx", from
        // `exec_inv`) + a var scoped to some other folder.
        db.upsert_env_var(custom_var("PB_TEST_SCOPED", "globalvalue123456789", None))
            .await
            .unwrap();
        db.upsert_env_var(custom_var("PB_TEST_SCOPED", "fldrvalue4321", Some("fx")))
            .await
            .unwrap();
        db.upsert_env_var(custom_var(
            "PB_TEST_FOREIGN",
            "foreignsecret555777",
            Some("other-folder"),
        ))
        .await
        .unwrap();

        // The folder value (len 13) wins over the global (len 20); the
        // foreign folder's var is not injected at all.
        let v = exec_sh(
            &db,
            "echo len=${#PB_TEST_SCOPED}; echo foreign=${PB_TEST_FOREIGN:-unset}",
        )
        .await;
        let stdout = v["stdout"].as_str().unwrap();
        assert!(stdout.contains("len=13"), "stdout: {stdout}");
        assert!(stdout.contains("foreign=unset"), "stdout: {stdout}");

        // Even a var another folder owns stays masked if its value ever
        // shows up in output.
        let v = exec_sh(&db, "echo foreignsecret555777").await;
        let stdout = v["stdout"].as_str().unwrap();
        assert!(!stdout.contains("foreignsecret555777"), "stdout: {stdout}");
        assert!(stdout.contains("********"), "stdout: {stdout}");
    }

    #[tokio::test]
    async fn ssh_key_list_impl_never_leaks_key_material() {
        let db = Db::in_memory().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        db.insert_ssh_key(crate::db::models::NewSshKey {
            id: "k1".to_string(),
            name: "prod box".to_string(),
            key_type: "ed25519".to_string(),
            public_key: "ssh-ed25519 AAAAC3...".to_string(),
            fingerprint: "SHA256:abc123".to_string(),
            private_key_ciphertext: "SUPER-SECRET-CIPHERTEXT".to_string(),
            private_key_nonce: "SECRET-NONCE-1".to_string(),
            passphrase_ciphertext: Some("SUPER-SECRET-PASSPHRASE-CIPHERTEXT".to_string()),
            passphrase_nonce: Some("SECRET-NONCE-2".to_string()),
            created_at: now.clone(),
            updated_at: now,
            created_by: None,
        })
        .await
        .unwrap();

        let out = ssh_key_list_impl(&db);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let keys = v["keys"].as_array().unwrap();
        assert_eq!(keys.len(), 1, "{out}");

        let mut field_names: Vec<&str> = keys[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        field_names.sort();
        assert_eq!(
            field_names,
            vec![
                "created_at",
                "fingerprint",
                "has_passphrase",
                "id",
                "key_type",
                "name",
            ],
            "unexpected field set: {out}"
        );
        assert_eq!(keys[0]["has_passphrase"], true);

        // Never the raw secret material, its ciphertext, or its nonce — not
        // even for a key that has a passphrase set.
        assert!(!out.contains("SUPER-SECRET-CIPHERTEXT"));
        assert!(!out.contains("SECRET-NONCE-1"));
        assert!(!out.contains("SUPER-SECRET-PASSPHRASE-CIPHERTEXT"));
        assert!(!out.contains("SECRET-NONCE-2"));
    }

    #[tokio::test]
    async fn ssh_key_list_host_fn_gate_reflects_granted_permissions() {
        let db = Db::in_memory().unwrap();
        let data_dir = tempfile::tempdir().unwrap();

        let ctx_for = |granted: &[&str]| {
            let permissions: Arc<std::sync::RwLock<std::collections::HashSet<String>>> = Arc::new(
                std::sync::RwLock::new(granted.iter().map(|p| p.to_string()).collect()),
            );
            let ud = UserData::new(HostState {
                db: db.clone(),
                data_dir: data_dir.path().to_path_buf(),
                plugin_id: "test".to_string(),
                permissions,
                invocation: Arc::new(std::sync::RwLock::new(None)),
                live: Arc::new(std::sync::RwLock::new(None)),
                user: Arc::new(std::sync::RwLock::new(None)),
                provider_runtime: Arc::new(
                    crate::provider::plugin_provider::PluginProviderRuntime::new(),
                ),
                pending_provider: Arc::new(std::sync::RwLock::new(None)),
                provider_registry: Arc::new(std::sync::RwLock::new(None)),
            });
            state_ssh_context(&ud).unwrap()
        };

        // `peckboard_ssh_key_list` reads exactly this tuple to decide
        // whether to serve the request — so this is the real gate, not just
        // a permission-set lookup.
        let (_, has_ssh, has_ssh_keys, _) = ctx_for(&[]);
        assert!(!has_ssh && !has_ssh_keys, "no permissions granted");

        let (_, has_ssh, has_ssh_keys, _) = ctx_for(&["ssh"]);
        assert!(has_ssh && !has_ssh_keys, "ssh only, not ssh_keys");

        let (_, has_ssh, has_ssh_keys, _) = ctx_for(&["ssh_keys"]);
        assert!(!has_ssh && has_ssh_keys, "ssh_keys only, not ssh");

        let (_, has_ssh, has_ssh_keys, _) = ctx_for(&["ssh", "ssh_keys"]);
        assert!(has_ssh && has_ssh_keys, "both granted");
    }

    /// `trusted_caller` (the proof-token constructor behind
    /// `peckboard_create_session` / `peckboard_dispatch_capture`) accepts
    /// exactly the two valid contexts and refuses everything else.
    #[tokio::test]
    async fn trusted_caller_requires_invocation_or_authed_user_authority() {
        let db = Db::in_memory().unwrap();
        let data_dir = tempfile::tempdir().unwrap();

        let ctx_for =
            |granted: &[&str], invocation: Option<InvocationContext>, user: Option<UserContext>| {
                let ud = UserData::new(HostState {
                    db: db.clone(),
                    data_dir: data_dir.path().to_path_buf(),
                    plugin_id: "test".to_string(),
                    permissions: Arc::new(std::sync::RwLock::new(
                        granted.iter().map(|p| p.to_string()).collect(),
                    )),
                    invocation: Arc::new(std::sync::RwLock::new(invocation)),
                    live: Arc::new(std::sync::RwLock::new(None)),
                    user: Arc::new(std::sync::RwLock::new(user)),
                    provider_runtime: Arc::new(
                        crate::provider::plugin_provider::PluginProviderRuntime::new(),
                    ),
                    pending_provider: Arc::new(std::sync::RwLock::new(None)),
                    provider_registry: Arc::new(std::sync::RwLock::new(None)),
                });
                state_permission_and_trusted_caller(&ud, "session_write").unwrap()
            };
        let user_ctx = || UserContext {
            user_id: "u1".into(),
            folder_id: Some("f1".into()),
            project_id: Some("p1".into()),
            session_id: None,
        };

        // Tool-invoke path: the token carries the invocation's scope.
        let (_, _, _, caller) =
            ctx_for(&["session_write"], Some(inv(Some("p1"), Some("f1"))), None);
        let caller = caller.expect("invocation context yields a token");
        assert!(!caller.authority);
        assert_eq!(caller.folder_id.as_deref(), Some("f1"));

        // Authed-request path: requires the `user_authority` grant.
        let (_, _, _, caller) =
            ctx_for(&["session_write", "user_authority"], None, Some(user_ctx()));
        let caller = caller.expect("authed user + user_authority yields a token");
        assert!(caller.authority);
        assert_eq!(caller.folder_id.as_deref(), Some("f1"));

        // Authed request WITHOUT the grant: refused.
        let (_, _, _, caller) = ctx_for(&["session_write"], None, Some(user_ctx()));
        assert!(caller.is_none(), "user context without user_authority");

        // Neither context: refused.
        let (_, _, _, caller) = ctx_for(&["session_write", "user_authority"], None, None);
        assert!(caller.is_none(), "no context at all");
    }

    /// Session calls work end-to-end under the authed-request token: creating
    /// a session and dispatching to it succeed with a user-derived
    /// `TrustedCaller` exactly as they do with a tool-invoke one.
    #[tokio::test]
    async fn session_calls_succeed_under_authed_user_token() {
        let db = setup().await; // folder f1 / project p1
        let user = UserContext {
            user_id: "u1".into(),
            folder_id: Some("f1".into()),
            project_id: Some("p1".into()),
            session_id: None,
        };
        let caller = TrustedCaller(user.as_invocation());

        let out = create_session_impl(&db, r#"{"name":"installer"}"#, &caller);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("error").is_none(), "create under authority: {out}");
        let sid = v["session"]["id"].as_str().unwrap().to_string();

        let live = Arc::new(RecordingLive::default());
        let live_dyn: Arc<dyn LiveHost> = live.clone();
        let d = dispatch_capture_impl(
            &db,
            &format!(r#"{{"session_id":"{sid}","prompt":"install"}}"#),
            &caller,
            Some(live_dyn),
        );
        assert!(d.contains("\"ok\":true"), "dispatch under authority: {d}");
        assert_eq!(*live.calls.lock().unwrap(), vec![format!("dispatch:{sid}")]);
    }

    /// `models_read` output shape: only thinking models are listed, ids stay
    /// account-qualified, and the response carries METADATA fields only — no
    /// credential-shaped keys.
    #[tokio::test]
    async fn list_models_filters_thinking_and_exposes_no_credentials() {
        use crate::provider::registry::{ProviderCapabilities, ProviderInfo, ProviderRegistry};
        use crate::provider::stream::ModelInfo;

        let db = Db::in_memory().unwrap();
        let registry = Arc::new(ProviderRegistry::new());
        registry
            .register(
                Arc::new(crate::provider::mock::MockProvider::new()),
                ProviderInfo {
                    id: "claude".into(),
                    display_name: "Claude".into(),
                    models: vec![
                        ModelInfo {
                            id: "claude-fable-5".into(),
                            display_name: "Claude Fable 5".into(),
                            capabilities: vec!["code".into(), "reasoning".into()],
                            tier: 4,
                        },
                        ModelInfo {
                            id: "claude-sonnet-4-6@acc_1".into(),
                            display_name: "[Work] Claude Sonnet 4.6".into(),
                            capabilities: vec!["code".into(), "reasoning".into()],
                            tier: 2,
                        },
                        ModelInfo {
                            id: "claude-haiku-4-5".into(),
                            display_name: "Claude Haiku 4.5".into(),
                            capabilities: vec!["code".into()], // NOT thinking
                            tier: 1,
                        },
                    ],
                    effort_levels: vec![],
                    capabilities: ProviderCapabilities::default(),
                },
            )
            .await;

        let out = list_models_impl(&db, Some(registry));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let models = v["models"].as_array().unwrap();
        let ids: Vec<&str> = models.iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert!(ids.contains(&"claude:claude-fable-5"), "{ids:?}");
        assert!(ids.contains(&"claude:claude-sonnet-4-6@acc_1"), "{ids:?}");
        // Non-thinking models are filtered SERVER-SIDE — absent, not flagged.
        assert!(!ids.iter().any(|i| i.contains("haiku")), "{ids:?}");

        // Account id comes from the last-`@` split; bare models carry null.
        let scoped = models
            .iter()
            .find(|m| m["id"] == "claude:claude-sonnet-4-6@acc_1")
            .unwrap();
        assert_eq!(scoped["account_id"], "acc_1");
        assert_eq!(scoped["provider"], "claude");
        assert_eq!(scoped["tier"], 2);
        let bare = models
            .iter()
            .find(|m| m["id"] == "claude:claude-fable-5")
            .unwrap();
        assert!(bare["account_id"].is_null());

        // Metadata only: the exact key set, nothing credential-shaped.
        for m in models {
            let mut keys: Vec<&str> = m.as_object().unwrap().keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                vec![
                    "account_id",
                    "display_name",
                    "id",
                    "provider",
                    "thinking",
                    "tier"
                ]
            );
            assert_eq!(m["thinking"], true);
        }

        // No registry bound → clean refusal, not a panic.
        let unbound = list_models_impl(&db, None);
        assert!(unbound.contains("error"), "{unbound}");
    }

    /// The `models_read` gate is the tuple `peckboard_list_models` actually
    /// reads — mirrored from the ssh gate test above.
    #[tokio::test]
    async fn list_models_host_fn_gate_reflects_models_read() {
        let db = Db::in_memory().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let ctx_for = |granted: &[&str]| {
            let ud = UserData::new(HostState {
                db: db.clone(),
                data_dir: data_dir.path().to_path_buf(),
                plugin_id: "test".to_string(),
                permissions: Arc::new(std::sync::RwLock::new(
                    granted.iter().map(|p| p.to_string()).collect(),
                )),
                invocation: Arc::new(std::sync::RwLock::new(None)),
                live: Arc::new(std::sync::RwLock::new(None)),
                user: Arc::new(std::sync::RwLock::new(None)),
                provider_runtime: Arc::new(
                    crate::provider::plugin_provider::PluginProviderRuntime::new(),
                ),
                pending_provider: Arc::new(std::sync::RwLock::new(None)),
                provider_registry: Arc::new(std::sync::RwLock::new(None)),
            });
            state_permission_and_registry(&ud, "models_read").unwrap()
        };
        let (_, _, ok, registry) = ctx_for(&[]);
        assert!(!ok, "no permissions granted");
        assert!(registry.is_none(), "no registry bound");
        let (_, _, ok, _) = ctx_for(&["models_read"]);
        assert!(ok, "models_read granted");
        let (_, _, ok, _) = ctx_for(&["session_read", "ssh"]);
        assert!(!ok, "unrelated permissions don't grant models_read");
    }

    #[tokio::test]
    async fn orchestrate_impls_work_without_caller_context() {
        let db = Db::in_memory().unwrap();
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "fO".into(),
            name: "Repo".into(),
            path: ".".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_session(crate::db::models::NewSession {
            id: "sO".into(),
            name: "Brain".into(),
            folder_id: "fO".into(),
            created_at: ts.clone(),
            last_activity: ts,
            ..Default::default()
        })
        .await
        .unwrap();

        // send: unknown target refuses before touching live.
        let rec = std::sync::Arc::new(RecordingLive::default());
        let r = orchestrate_send_impl(
            &db,
            r#"{"session_id":"nope","text":"go"}"#,
            Some(rec.clone()),
        );
        assert!(r.contains("error"), "unknown target: {r}");
        assert!(rec.calls.lock().unwrap().is_empty());
        // Blank text refuses; no live host refuses; the happy path records.
        let r = orchestrate_send_impl(&db, r#"{"session_id":"sO","text":"  "}"#, Some(rec.clone()));
        assert!(r.contains("error"), "blank text: {r}");
        let r = orchestrate_send_impl(&db, r#"{"session_id":"sO","text":"go"}"#, None);
        assert!(r.contains("live control unavailable"), "no live: {r}");
        let r = orchestrate_send_impl(&db, r#"{"session_id":"sO","text":"go"}"#, Some(rec.clone()));
        assert!(r.contains("\"ok\":true"), "send ok: {r}");
        assert_eq!(rec.calls.lock().unwrap().as_slice(), ["send:sO:go"]);

        // create: unknown folder refuses; a real folder creates the session.
        let r = orchestrate_create_session_impl(&db, r#"{"folder_id":"nope","name":"W"}"#);
        assert!(r.contains("folder not found"), "bad folder: {r}");
        let r = orchestrate_create_session_impl(
            &db,
            r#"{"folder_id":"fO","name":"Worker","system_prompt":"wear the QA hat"}"#,
        );
        assert!(r.contains("\"session\""), "create ok: {r}");
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        let created_id = v["session"]["id"].as_str().unwrap().to_string();
        let created = db.get_session_blocking(&created_id).unwrap().unwrap();
        assert_eq!(created.folder_id, "fO");
        assert_eq!(created.system_prompt.as_deref(), Some("wear the QA hat"));

        // set_prompt: set then clear.
        let r = orchestrate_set_prompt_impl(
            &db,
            r#"{"session_id":"sO","system_prompt":"you are the orchestrator"}"#,
        );
        assert!(r.contains("\"system_prompt_set\":true"), "set: {r}");
        let r = orchestrate_set_prompt_impl(&db, r#"{"session_id":"sO"}"#);
        assert!(r.contains("\"system_prompt_set\":false"), "clear: {r}");

        // session_state: exists both ways, no error envelope for a miss.
        let r = orchestrate_session_state_impl(&db, r#"{"session_id":"sO"}"#);
        assert!(r.contains("\"exists\":true"), "state: {r}");
        assert!(r.contains("\"folder_id\":\"fO\""), "state folder: {r}");
        let r = orchestrate_session_state_impl(&db, r#"{"session_id":"gone"}"#);
        assert!(r.contains("\"exists\":false"), "miss: {r}");
    }
}
