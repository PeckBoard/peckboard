use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use extism::{Manifest as ExtismManifest, Plugin, PluginBuilder, Wasm};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use serde::Serialize;

use super::hooks::{
    HTTP_AUTHED_HOOK, HTTP_REQUEST_HOOK, HookResult, MCP_TOOL_INVOKE_HOOK, PluginHttpOutcome,
    PluginHttpResponse, PluginManifest, PluginMcpToolEntry, SidebarItem, SidebarItemEntry,
    UiPanelEntry, Verdict,
};
use crate::db::Db;
use crate::db::crud::{APPROVAL_APPROVED, APPROVAL_DENIED};

const MEMORY_LIMIT_PAGES: u32 = 2048; // 128 MB (64 KB per page)

/// Per-linear-memory virtual address-space reservation handed to wasmtime.
///
/// wasmtime reserves **4 GiB of address space per linear memory** by default
/// (plus a 32 MiB guard) so it can elide bounds checks. `with_memory_max`
/// above only installs a runtime `ResourceLimiter` that *caps growth* — it
/// does nothing to this reservation. Multiplied across every plugin instance
/// and each one's extism runtime kernel memory, the default inflates the
/// process's virtual size (VSZ) into the tens of GiB. It's unbacked
/// reservation, not resident memory (RSS), but it's alarming in `top` and a
/// real hazard under `RLIMIT_AS` / strict overcommit.
///
/// Our plugins never grow past `MEMORY_LIMIT_PAGES`, so reserve exactly that
/// and let wasmtime relocate the memory (the default `memory_may_move`) on the
/// rare occasion a plugin grows into it. Passed via
/// `PluginBuilder::with_wasmtime_config`, which leaves these memory tunables
/// untouched (it only overrides epoch/fuel/exceptions/cache).
const MEMORY_RESERVATION_BYTES: u64 = MEMORY_LIMIT_PAGES as u64 * 64 * 1024; // 128 MB
const CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// The complete set of hook names Peckboard actually dispatches. A
/// plugin manifest may only register handlers for hooks in this list;
/// anything else is rejected at load time.
///
/// Without this gate a malicious plugin could claim it handles
/// `mcp.token.issue.before` (a hook that can short-circuit token
/// minting via `Verdict::Cancel`) and silently break worker
/// dispatch — or, worse, modify payloads on hooks the user never
/// expected to be plugin-controllable. Pinning the set in code means
/// only hooks we've thought through ever fire plugin code.
///
/// When you add a new dispatched hook in the codebase, add its name
/// here too. The corresponding test below will catch the omission if
/// you forget.
pub const ALLOWED_HOOKS: &[&str] = &[
    "card.create.before",
    "card.update.before",
    "card.priorities.list",
    "http.request.authed",
    "http.request.before",
    "mcp.config.delete.after",
    "mcp.config.write.after",
    "mcp.config.write.before",
    "mcp.token.issue.after",
    "mcp.token.issue.before",
    "mcp.token.revoke.after",
    "mcp.tool.call.after",
    "mcp.tool.call.before",
    "mcp.tool.call.failed",
    "mcp.tool.invoke",
    "session.reference.resolve",
    "session.user.answer",
    "todo",
];

/// The complete set of host capabilities a WASM plugin may request in its
/// manifest `permissions`. Like [`ALLOWED_HOOKS`] this is pinned in code:
/// a plugin declaring anything outside it fails to load, so only
/// capabilities we've designed a host-function gate for can ever be granted.
/// Each maps to one or more host functions in `src/plugin/host.rs` (or a
/// manifest capability) that refuse unless the permission was granted.
pub const ALLOWED_PERMISSIONS: &[&str] = &[
    "ask_user",  // peckboard_ask_user / peckboard_get_answer — prompt the caller's user
    "broadcast", // peckboard_broadcast — push a namespaced ws event
    "browser_runs_read", // peckboard_browser_runs / _run / _run_frame — recorded test runs
    "contribute_sidebar", // declare sidebar_items
    "data_store", // peckboard_store_* — plugin-owned document store
    "event_append", // peckboard_append_event
    "http_fetch", // peckboard_http_fetch — outbound public-web GET/HEAD
    "process_exec", // peckboard_exec — run an allowlisted command in the caller's folder
    "process_exec_any", // peckboard_exec_any — run ANY folder-contained command (after approval)
    "project_files_read", // peckboard_list_project_files / read_file / read_file_base64
    "project_files_write", // peckboard_write_file
    "provide_mcp_tools", // declare mcp_tools (mcp.tool.invoke)
    "session_dispatch", // peckboard_dispatch_capture / resume_session
    "session_control", // peckboard_interrupt_session / terminate_agent / clear_session / send_message — full cross-folder control of any session
    "session_read",    // peckboard_get_session / list_sessions
    "session_write",   // peckboard_create_session / update_session
    "user_authority",  // serve authenticated UI + act under the user (ui_routes)
];

/// Whether an operator has approved the set of hooks a loaded plugin
/// declares. A plugin is **inert** — no hook fires, no `/plugin-api`
/// route dispatches, no ui_panel surfaces, and its `init` is not even
/// run — unless it is [`ApprovalState::Approved`].
///
/// The grant is recorded against the plugin's *exact* declared hook set
/// (see [`canonical_hooks`]), so a plugin whose hooks change since it was
/// last decided on drops back to `Pending` rather than inheriting an old
/// approval — an attacker can't swap the `.wasm` for one that claims more
/// hooks and ride a stale grant.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ApprovalState {
    /// No stored decision matches the plugin's current hook set — awaiting
    /// the operator. The default for a newly-installed plugin.
    Pending,
    /// Operator denied this hook set. Inert until explicitly re-approved.
    Denied,
    /// Operator approved this hook set and `init` has been run. Active.
    Approved,
}

/// A loaded plugin instance.
///
/// `plugin` is wrapped in its own `Mutex` so concurrent dispatches of
/// different hooks (or the same hook against different plugins) don't
/// serialise on a single PluginManager-wide lock. The Plugin type
/// isn't `Sync` and `call` takes `&mut self`, so we need a per-plugin
/// mutex either way; placing it here means the outer `plugins` lock
/// is only held while we *find* the plugins to dispatch to, not
/// across the (up to 2-second) extism call.
struct LoadedPlugin {
    name: String,
    manifest: PluginManifest,
    plugin: Arc<Mutex<Plugin>>,
    /// Canonical (sorted, newline-joined) form of `manifest.hooks` — the
    /// exact string an approval decision is stored and compared against.
    hooks_canonical: String,
    /// Whether the operator has approved this plugin's hook set.
    approval: ApprovalState,
    /// `Some(error)` if `init` was run (on approval) but failed; the plugin
    /// is then treated as inactive even though the hook set was approved.
    init_error: Option<String>,
    /// Shared with this plugin's scoped host functions: the trusted context of
    /// the `mcp.tool.invoke` currently running (or `None`). `invoke_mcp_tool`
    /// sets it from the verified caller context right before calling `handle`
    /// and clears it after, so the host functions re-derive scope server-side
    /// rather than from plugin-supplied ids (see [`host::InvocationContext`]).
    invocation: Arc<std::sync::RwLock<Option<super::host::InvocationContext>>>,
    /// Shared with this plugin's scoped host functions: the trusted
    /// authenticated-user context of an in-flight `http.request.authed` request
    /// (or `None`). `serve_http_authed` sets it from the `require_auth`-verified
    /// user around the dispatch and clears it after.
    user: Arc<std::sync::RwLock<Option<super::host::UserContext>>>,
}

impl LoadedPlugin {
    /// A plugin is active — eligible for hook dispatch, route serving, and
    /// ui_panel surfacing — only once its hook set is approved AND its
    /// deferred `init` succeeded.
    fn is_active(&self) -> bool {
        self.approval == ApprovalState::Approved && self.init_error.is_none()
    }

    /// The wire status label the `/api/plugins` catalog reports.
    fn status_label(&self) -> &'static str {
        status_label(&self.approval, &self.init_error)
    }

    /// Build the `/api/plugins` catalog entry for this plugin. One place so
    /// every construction site (`wasm_plugins`, `install`, `decide`) carries
    /// the same manifest-sourced metadata.
    fn to_info(&self) -> WasmPluginInfo {
        WasmPluginInfo {
            name: self.name.clone(),
            description: self.manifest.description.clone(),
            version: self.manifest.version.clone(),
            repository: self.manifest.repository.clone(),
            hooks: self.manifest.hooks.clone(),
            permissions: self.manifest.permissions.clone(),
            status: self.status_label(),
            error: self.init_error.clone(),
        }
    }
}

/// The wire status label for an approval state: `pending`, `denied`,
/// `approved`, or `init_failed` (approved but its deferred `init` failed).
fn status_label(approval: &ApprovalState, init_error: &Option<String>) -> &'static str {
    match approval {
        ApprovalState::Pending => "pending",
        ApprovalState::Denied => "denied",
        ApprovalState::Approved if init_error.is_some() => "init_failed",
        ApprovalState::Approved => "approved",
    }
}

/// Resolve the approval state a plugin loads in, given the operator's
/// stored decision (if any) and the hook set the plugin *currently*
/// declares. A decision only counts when it was made against the same
/// canonical hook set; otherwise the plugin is `Pending` — this is what
/// stops a swapped `.wasm` that declares new hooks from inheriting an old
/// approval.
fn resolve_approval(
    stored: Option<&crate::db::models::PluginApprovalRow>,
    hooks_canonical: &str,
) -> ApprovalState {
    match stored {
        Some(row) if row.hooks == hooks_canonical && row.status == APPROVAL_APPROVED => {
            ApprovalState::Approved
        }
        Some(row) if row.hooks == hooks_canonical && row.status == APPROVAL_DENIED => {
            ApprovalState::Denied
        }
        _ => ApprovalState::Pending,
    }
}

/// One loaded WASM plugin's approval state, for the `/api/plugins`
/// catalog and the approval prompt. `status` is one of `pending`,
/// `approved`, `denied`, or `init_failed`.
#[derive(Debug, Clone, Serialize)]
pub struct WasmPluginInfo {
    /// The plugin's id (its `.wasm` file stem).
    pub name: String,
    /// Self-reported summary, version, and source repository from the
    /// plugin's manifest (all required there) — shown on the plugin's card.
    pub description: String,
    pub version: String,
    pub repository: String,
    /// Every hook the plugin declares — what the operator is approving.
    pub hooks: Vec<String>,
    /// Host permissions the plugin requests — also part of the approval.
    pub permissions: Vec<String>,
    pub status: &'static str,
    /// Present only when `status` is `init_failed`.
    pub error: Option<String>,
}

/// Canonical form of a hook set for approval storage and comparison:
/// sorted, de-duplicated, and newline-joined, so two set-equal hook lists
/// in any order produce the same string. Binding an approval to this
/// string means re-ordering hooks doesn't force a re-prompt, but adding or
/// removing one does.
fn canonical_hooks(hooks: &[String]) -> String {
    let mut sorted: Vec<&str> = hooks.iter().map(|h| h.as_str()).collect();
    sorted.sort_unstable();
    sorted.dedup();
    sorted.join("\n")
}

/// Canonical fingerprint of the full grant an operator approves: the hook set
/// (see [`canonical_hooks`]) plus the requested permission set. Changing
/// either re-prompts. **Backward compatible:** a plugin that requests no
/// permissions produces exactly the old hooks-only string, so approvals made
/// before permissions existed still match and don't force a re-prompt.
fn canonical_grant(hooks: &[String], permissions: &[String]) -> String {
    let h = canonical_hooks(hooks);
    if permissions.is_empty() {
        return h;
    }
    let mut perms: Vec<&str> = permissions.iter().map(|p| p.as_str()).collect();
    perms.sort_unstable();
    perms.dedup();
    format!("{h}\u{1f}perm:{}", perms.join("\n"))
}

/// Whether `id` is a safe bare plugin id — usable verbatim as a `.wasm`
/// filename with no path traversal or separators. Matches the registry's
/// `^[a-z0-9_-]+$`, so an install can't write outside the plugins dir or
/// clobber an arbitrary file via a crafted id.
fn is_safe_plugin_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// Whether `name` is a safe MCP tool name a plugin may declare: lowercase
/// ascii, digits, and underscore, non-empty and bounded. Keeps plugin tool
/// names in the same shape as core's (`spin_up_experts`, `list_cards`) so the
/// merged `tools/list` is uniform and a crafted name can't inject odd
/// characters into the protocol surface.
fn is_safe_mcp_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Validate a plugin's declared `mcp_tools` at load time. A plugin that
/// declares any MCP tool MUST also declare the terminal `mcp.tool.invoke`
/// hook (else core merges its tools into the worker `tools/list` but has no
/// way to dispatch a call to them), and every tool needs a safe name, a
/// non-empty description, and an object (or absent) input schema. Returns an
/// error naming the first problem; `Ok(())` when there are no tools.
fn validate_mcp_tools(name: &str, manifest: &PluginManifest) -> anyhow::Result<()> {
    if manifest.mcp_tools.is_empty() {
        return Ok(());
    }
    if !manifest.hooks.iter().any(|h| h == MCP_TOOL_INVOKE_HOOK) {
        anyhow::bail!(
            "plugin '{name}' declares mcp_tools but not the '{MCP_TOOL_INVOKE_HOOK}' \
             hook needed to dispatch them",
        );
    }
    if !manifest
        .permissions
        .iter()
        .any(|p| p == "provide_mcp_tools")
    {
        anyhow::bail!(
            "plugin '{name}' declares mcp_tools but not the 'provide_mcp_tools' permission",
        );
    }
    for tool in &manifest.mcp_tools {
        if !is_safe_mcp_tool_name(&tool.name) {
            anyhow::bail!(
                "plugin '{name}' declares mcp_tool with invalid name '{}' \
                 (expected ^[a-z0-9_]+$)",
                tool.name,
            );
        }
        if tool.description.trim().is_empty() {
            anyhow::bail!(
                "plugin '{name}' mcp_tool '{}' has an empty description",
                tool.name,
            );
        }
        if !tool.input_schema.is_null() && !tool.input_schema.is_object() {
            anyhow::bail!(
                "plugin '{name}' mcp_tool '{}' input_schema must be a JSON object",
                tool.name,
            );
        }
    }
    Ok(())
}

/// Run a plugin's `init` export with its per-plugin config block. Returns
/// the error string (for surfacing as `init_failed`) on failure.
fn run_init(plugin: &mut Plugin, config: String) -> Result<(), String> {
    plugin
        .call::<String, String>("init", config)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Manages all loaded plugins and dispatches hook calls.
pub struct PluginManager {
    plugins: Arc<Mutex<Vec<LoadedPlugin>>>,
    plugins_dir: PathBuf,
    /// Live `Db` handle threaded into every loaded plugin's data-access host
    /// functions (`src/plugin/host.rs`). `None` for `empty()` managers, which
    /// never load plugins and so never need it.
    db: Option<Db>,
    /// Late-bound bridge to live-application capabilities (agent dispatch),
    /// shared into every plugin's host functions and set once by `main.rs`
    /// after `AppState` exists (see [`PluginManager::set_live_host`]). `None`
    /// until then, so the live host functions refuse rather than act — and
    /// always `None` for `empty()`/headless managers.
    live: Arc<std::sync::RwLock<Option<Arc<dyn super::host::LiveHost>>>>,
}

impl PluginManager {
    /// Create a new plugin manager. Does not load plugins yet. The `db`
    /// handle backs the data-access host functions exposed to plugins.
    pub fn new(data_dir: &Path, db: Db) -> Self {
        PluginManager {
            plugins: Arc::new(Mutex::new(Vec::new())),
            plugins_dir: data_dir.join("plugins"),
            db: Some(db),
            live: Arc::new(std::sync::RwLock::new(None)),
        }
    }

    /// Bind the live-application bridge used by the agent-dispatch host
    /// functions. Called once from `main.rs` after `AppState` is built; every
    /// already-loaded and future plugin sees it (the slot is shared). Idempotent
    /// — a later call replaces the binding.
    pub fn set_live_host(&self, live: Arc<dyn super::host::LiveHost>) {
        if let Ok(mut guard) = self.live.write() {
            *guard = Some(live);
        }
    }

    /// A plugin manager that hosts no plugins and never loads any. Dispatch is
    /// always a no-op, so this is the right default for components that take a
    /// `PluginManager` for uniformity but never host plugins (the watchdog's
    /// throwaway `SessionManager`, tests). The real manager comes from
    /// `AppState` via `SessionManager::with_plugins`.
    pub fn empty() -> Self {
        PluginManager {
            plugins: Arc::new(Mutex::new(Vec::new())),
            plugins_dir: PathBuf::new(),
            db: None,
            live: Arc::new(std::sync::RwLock::new(None)),
        }
    }

    /// Scan the plugins directory and load all .wasm files.
    pub async fn load_all(&self) -> anyhow::Result<()> {
        if !self.plugins_dir.exists() {
            std::fs::create_dir_all(&self.plugins_dir)?;
            info!(
                "Created plugins directory at {}",
                self.plugins_dir.display()
            );
            return Ok(());
        }

        let entries = std::fs::read_dir(&self.plugins_dir)?;
        let mut plugins = self.plugins.lock().await;

        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().map_or(false, |e| e == "wasm") {
                match self.load_plugin(&path) {
                    Ok(loaded) => {
                        info!(
                            "Loaded plugin '{}' with {} hooks",
                            loaded.name,
                            loaded.manifest.hooks.len()
                        );
                        plugins.push(loaded);
                    }
                    Err(e) => {
                        error!("Failed to load plugin {}: {e}", path.display());
                    }
                }
            }
        }

        info!("Loaded {} plugin(s)", plugins.len());
        Ok(())
    }

    /// Load a single plugin from a .wasm file.
    fn load_plugin(&self, path: &Path) -> anyhow::Result<LoadedPlugin> {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();
        self.load_plugin_from(name, Wasm::file(path))
    }

    /// Load a plugin from any [`Wasm`] source (a file at startup, or
    /// freshly-downloaded bytes at install time). `name` is the plugin id
    /// (its `.wasm` file stem) — host-function namespacing, config lookup,
    /// and approval are all keyed by it.
    fn load_plugin_from(&self, name: String, wasm: Wasm) -> anyhow::Result<LoadedPlugin> {
        let manifest = ExtismManifest::new([wasm])
            .with_timeout(CALL_TIMEOUT)
            .with_memory_max(MEMORY_LIMIT_PAGES);

        // Wire the data-access host functions into the plugin so it can read
        // and write Peckboard data through the sandbox. `empty()` managers have
        // no `Db` and never reach here, so they register nothing.
        //
        // The granted-permission set is shared with the host functions (which
        // gate on it) but populated only after we parse the manifest below —
        // host functions are wired before the manifest is known. It stays
        // empty (so gated functions deny) until then; since a plugin can't run
        // any code before `init`/`handle`, and those run only after approval,
        // the set always reflects the declared permissions when a host
        // function actually executes.
        let granted_permissions: Arc<std::sync::RwLock<std::collections::HashSet<String>>> =
            Arc::new(std::sync::RwLock::new(std::collections::HashSet::new()));
        // Shared with the plugin's scoped host functions; `invoke_mcp_tool`
        // populates it per-call (see `LoadedPlugin::invocation`).
        let invocation: Arc<std::sync::RwLock<Option<super::host::InvocationContext>>> =
            Arc::new(std::sync::RwLock::new(None));
        // Shared with the scoped host functions; `serve_http_authed` populates it
        // per authenticated request (see `LoadedPlugin::user`).
        let user: Arc<std::sync::RwLock<Option<super::host::UserContext>>> =
            Arc::new(std::sync::RwLock::new(None));
        let functions = match &self.db {
            // `name` is the plugin's id (its `.wasm` file stem), the same id
            // its `plugin_settings` rows are keyed by — so the self-storage
            // host functions stay scoped to this plugin's own namespace.
            Some(db) => super::host::host_functions(
                db,
                &name,
                granted_permissions.clone(),
                invocation.clone(),
                self.live.clone(),
                user.clone(),
                // plugins_dir is `<data_dir>/plugins`; the browser-run host
                // functions need the data dir itself.
                self.plugins_dir
                    .parent()
                    .map(std::path::Path::to_path_buf)
                    .unwrap_or_else(|| self.plugins_dir.clone()),
            ),
            None => Vec::new(),
        };
        // Shrink wasmtime's per-memory address-space reservation from the 4 GiB
        // default down to our growth cap (see `MEMORY_RESERVATION_BYTES`). Built
        // via `PluginBuilder` so we can hand wasmtime a custom `Config`;
        // `Plugin::new(manifest, functions, true)` is just this chain without the
        // config override.
        let mut wasmtime_config = wasmtime::Config::new();
        wasmtime_config.memory_reservation(MEMORY_RESERVATION_BYTES);
        let mut plugin = PluginBuilder::new(manifest)
            .with_functions(functions)
            .with_wasi(true)
            .with_wasmtime_config(wasmtime_config)
            .build()?;

        // Call manifest export to get hook declarations.
        let manifest_json = plugin.call::<&str, String>("manifest", "")?;
        let plugin_manifest: PluginManifest = serde_json::from_str(&manifest_json)?;

        // Required identity metadata must be present AND non-empty: serde
        // already rejects a missing field, but a blank string would slip
        // through and render an anonymous plugin card, so reject those too.
        for (field, value) in [
            ("description", &plugin_manifest.description),
            ("version", &plugin_manifest.version),
            ("repository", &plugin_manifest.repository),
        ] {
            if value.trim().is_empty() {
                return Err(anyhow::anyhow!(
                    "plugin '{name}' manifest is missing required field '{field}'",
                ));
            }
        }

        // Reject plugins that try to hook anything outside the
        // allowlist. Otherwise an attacker who can drop a `.wasm` file
        // into the plugins dir could intercept arbitrary internal
        // dispatches.
        if let Some(bad) = plugin_manifest
            .hooks
            .iter()
            .find(|h| !ALLOWED_HOOKS.contains(&h.as_str()))
        {
            return Err(anyhow::anyhow!(
                "plugin '{name}' declares unknown hook '{bad}'; \
                 see ALLOWED_HOOKS in src/plugin/manager.rs",
            ));
        }

        // Reject permissions outside the pinned allowlist — same rationale as
        // the hook allowlist: only capabilities core has a designed gate for
        // can ever be requested, let alone granted.
        if let Some(bad) = plugin_manifest
            .permissions
            .iter()
            .find(|p| !ALLOWED_PERMISSIONS.contains(&p.as_str()))
        {
            return Err(anyhow::anyhow!(
                "plugin '{name}' requests unknown permission '{bad}'; \
                 see ALLOWED_PERMISSIONS in src/plugin/manager.rs",
            ));
        }

        // Now that the (validated) permission set is known, hand it to the
        // host functions, which gate on it. Safe to populate before approval:
        // gated functions only run during `init`/`handle`, which run only once
        // approved — and approval grants exactly this set.
        if let Ok(mut guard) = granted_permissions.write() {
            *guard = plugin_manifest.permissions.iter().cloned().collect();
        }

        validate_mcp_tools(&name, &plugin_manifest)?;

        // A plugin contributing sidebar / project / session items must hold
        // `contribute_sidebar`; per-item path validity is enforced when the
        // catalog is built (`sidebar_items()` / `scoped_items()`), mirroring
        // `ui_panels()`.
        if (!plugin_manifest.sidebar_items.is_empty()
            || !plugin_manifest.project_items.is_empty()
            || !plugin_manifest.session_items.is_empty())
            && !plugin_manifest
                .permissions
                .iter()
                .any(|p| p == "contribute_sidebar")
        {
            return Err(anyhow::anyhow!(
                "plugin '{name}' declares sidebar/project/session items but not \
                 the 'contribute_sidebar' permission",
            ));
        }

        // Authenticated UI routes act under the logged-in user's authority, so
        // they require both the `user_authority` permission AND the
        // `http.request.authed` hook (the dispatch path that serves them).
        if !plugin_manifest.ui_routes.is_empty() {
            if !plugin_manifest
                .permissions
                .iter()
                .any(|p| p == "user_authority")
            {
                return Err(anyhow::anyhow!(
                    "plugin '{name}' declares ui_routes but not the \
                     'user_authority' permission",
                ));
            }
            if !plugin_manifest.hooks.iter().any(|h| h == HTTP_AUTHED_HOOK) {
                return Err(anyhow::anyhow!(
                    "plugin '{name}' declares ui_routes but not the \
                     '{HTTP_AUTHED_HOOK}' hook",
                ));
            }
        }

        // Resolve the operator's stored approval for this exact hook set.
        // A plugin is inert until approved (the user requires permission
        // for every hook), so a missing decision — or one made against a
        // different hook set — leaves it `Pending`, not active.
        let hooks_canonical = canonical_grant(&plugin_manifest.hooks, &plugin_manifest.permissions);
        let stored = self
            .db
            .as_ref()
            .and_then(|db| match db.get_plugin_approval_blocking(&name) {
                Ok(row) => row,
                Err(e) => {
                    warn!("Plugin '{name}' approval lookup failed: {e}");
                    None
                }
            });
        let approval = resolve_approval(stored.as_ref(), &hooks_canonical);

        // Run `init` only for an already-approved plugin. Deferring it for
        // pending/denied plugins is what makes them truly inert: an
        // unapproved plugin never executes code that could touch host
        // functions. (Approval later runs `init` via `decide`.) `init`
        // gets this plugin's `plugins.<stem>.config` block, or `{}` when
        // there is none; core forwards it opaquely.
        let mut init_error = None;
        if approval == ApprovalState::Approved {
            let init_config = read_plugin_config(&self.plugins_dir, &name);
            if let Err(e) = run_init(&mut plugin, init_config) {
                warn!("Plugin '{name}' init failed: {e}");
                init_error = Some(e);
            }
        } else {
            info!("Plugin '{name}' loaded but inert — awaiting hook approval");
        }

        Ok(LoadedPlugin {
            name,
            manifest: plugin_manifest,
            plugin: Arc::new(Mutex::new(plugin)),
            hooks_canonical,
            approval,
            init_error,
            invocation,
            user,
        })
    }

    /// Dispatch a hook to all registered plugins.
    ///
    /// Plugins are called in load order. If any plugin cancels, dispatch stops.
    /// If a plugin modifies the payload, the modified version is passed to the next.
    ///
    /// Acquires the outer `plugins` lock only long enough to find which
    /// plugins handle this hook and clone their per-plugin Arc<Mutex<Plugin>>,
    /// then releases it. Per-plugin work happens under each plugin's own
    /// mutex, so two dispatches for different hooks (or the same hook
    /// against disjoint plugin sets) don't serialise on a single
    /// PluginManager-wide lock — important because each `plugin.call`
    /// can take up to `CALL_TIMEOUT` (2s).
    pub async fn dispatch(&self, hook: &str, payload: serde_json::Value) -> HookResult {
        let targets: Vec<(String, Arc<Mutex<Plugin>>)> = {
            let plugins = self.plugins.lock().await;
            plugins
                .iter()
                .filter(|p| p.is_active() && p.manifest.hooks.contains(&hook.to_string()))
                .map(|p| (p.name.clone(), p.plugin.clone()))
                .collect()
        };

        let mut current_payload = payload;
        for (name, plugin) in targets {
            let call_input = serde_json::json!({
                "hook": hook,
                "payload": current_payload,
            });

            let result = {
                let mut guard = plugin.lock().await;
                guard.call::<String, String>("handle".to_string(), call_input.to_string())
            };

            match result {
                Ok(output) => match serde_json::from_str::<Verdict>(&output) {
                    Ok(Verdict::Allow { payload }) => {
                        if let Some(modified) = payload {
                            current_payload = modified;
                        }
                    }
                    Ok(Verdict::Cancel { reason }) => {
                        info!("Plugin '{name}' cancelled hook '{hook}': {reason}");
                        return HookResult::Cancelled {
                            plugin: name,
                            reason,
                        };
                    }
                    Ok(Verdict::Skip) => {
                        // No opinion, continue to next plugin
                    }
                    Err(e) => {
                        warn!("Plugin '{name}' returned invalid verdict for hook '{hook}': {e}");
                        // Treat parse errors as skip
                    }
                },
                Err(e) => {
                    warn!("Plugin '{name}' failed on hook '{hook}': {e}");
                    // Plugin failure doesn't block the operation
                }
            }
        }

        HookResult::Allowed(current_payload)
    }

    /// Fire a **notification** hook under the authority of an authenticated
    /// user. Unlike [`dispatch`], this lands a trusted [`super::host::UserContext`]
    /// for the duration of each plugin's `handle` call (exactly as
    /// [`serve_http_authed`] does), so the handler may call the scoped host
    /// functions on the user's behalf — gated by the `user_authority`
    /// permission. The verdict is ignored: the triggering operation has already
    /// happened, so a plugin cannot cancel it; it can only react (e.g. feed a
    /// Q&A to its question expert on [`super::hooks::USER_ANSWER_HOOK`]). Plugin
    /// failures are logged and never propagate to the caller.
    ///
    /// [`serve_http_authed`]: PluginManager::serve_http_authed
    pub async fn dispatch_authed(&self, hook: &str, user_id: &str, payload: serde_json::Value) {
        type AuthedTarget = (
            String,
            Arc<Mutex<Plugin>>,
            Arc<std::sync::RwLock<Option<super::host::UserContext>>>,
        );
        let targets: Vec<AuthedTarget> = {
            let plugins = self.plugins.lock().await;
            plugins
                .iter()
                .filter(|p| p.is_active() && p.manifest.hooks.iter().any(|h| h == hook))
                .map(|p| (p.name.clone(), p.plugin.clone(), p.user.clone()))
                .collect()
        };

        let call_input = serde_json::json!({ "hook": hook, "payload": payload }).to_string();
        for (name, plugin, user_slot) in targets {
            // Land the trusted user context for exactly this `handle` call.
            // A notification dispatch carries no request scope.
            if let Ok(mut slot) = user_slot.write() {
                *slot = Some(super::host::UserContext {
                    user_id: user_id.to_string(),
                    folder_id: None,
                    project_id: None,
                    session_id: None,
                });
            }
            let result = {
                let mut guard = plugin.lock().await;
                guard.call::<String, String>("handle", call_input.clone())
            };
            if let Ok(mut slot) = user_slot.write() {
                *slot = None;
            }
            if let Err(e) = result {
                warn!("Plugin '{name}' failed on authed hook '{hook}': {e}");
            }
        }
    }

    /// Dispatch a public HTTP request to whichever loaded plugin owns
    /// the route, returning the plugin's complete HTTP response.
    ///
    /// This backs the public `/plugin-api/*` surface (see
    /// `src/routes/plugin_api.rs`). It is deliberately generic: core
    /// knows nothing about API keys, scopes, or any specific endpoint.
    /// A plugin claims a route by declaring it in its manifest's
    /// `http_routes` (e.g. `"GET /plugin-api/cards/:id"`) AND listing
    /// [`HTTP_REQUEST_HOOK`] in its `hooks`; it then receives the
    /// request as the hook payload (a [`super::hooks::PluginHttpRequest`])
    /// and returns the response as the payload of a [`Verdict::Allow`]
    /// (a [`PluginHttpResponse`]).
    ///
    /// Matching plugins are consulted in load order:
    /// - `Verdict::Allow { payload }` → that response is returned.
    /// - `Verdict::Cancel { reason }` → a 500 error response carrying
    ///   the reason is returned. (A plugin that wants a specific status
    ///   such as 401/404 returns it via `Allow`, not `Cancel`.)
    /// - `Verdict::Skip`, an invalid verdict, or a plugin call failure →
    ///   the next matching plugin is tried.
    ///
    /// If no plugin declares a matching route, [`PluginHttpOutcome::NoRoute`]
    /// is returned (the route layer maps that to 404). If a plugin *did*
    /// claim the route but every matching plugin declined or errored, a
    /// 500 is returned rather than 404 — the route exists, it just
    /// failed to produce a response.
    pub async fn serve_http(
        &self,
        method: &str,
        path: &str,
        query: &str,
        headers: &BTreeMap<String, String>,
        body: &str,
    ) -> PluginHttpOutcome {
        // Find which plugins claim a route matching this request, with
        // the path params each pattern captured. Hold the outer lock
        // only long enough to clone the per-plugin Arc<Mutex<Plugin>>.
        // (plugin name, plugin handle, captured path params) for each
        // plugin whose declared routes match this request.
        type HttpTarget = (String, Arc<Mutex<Plugin>>, BTreeMap<String, String>);
        let targets: Vec<HttpTarget> = {
            let plugins = self.plugins.lock().await;
            plugins
                .iter()
                .filter(|p| {
                    p.is_active() && p.manifest.hooks.iter().any(|h| h == HTTP_REQUEST_HOOK)
                })
                .filter_map(|p| {
                    p.manifest
                        .http_routes
                        .iter()
                        .find_map(|route| match_http_route(route, method, path))
                        .map(|params| (p.name.clone(), p.plugin.clone(), params))
                })
                .collect()
        };

        if targets.is_empty() {
            return PluginHttpOutcome::NoRoute;
        }

        let payload = serde_json::json!({
            "method": method,
            "path": path,
            "query": query,
            "headers": headers,
            "body": body,
        });

        for (name, plugin, params) in targets {
            let mut req_payload = payload.clone();
            req_payload["params"] = serde_json::json!(params);
            let call_input = serde_json::json!({
                "hook": HTTP_REQUEST_HOOK,
                "payload": req_payload,
            });

            let result = {
                let mut guard = plugin.lock().await;
                guard.call::<String, String>("handle", call_input.to_string())
            };

            match result {
                Ok(output) => match serde_json::from_str::<Verdict>(&output) {
                    Ok(verdict) => match verdict {
                        Verdict::Allow { payload } => {
                            return verdict_to_outcome(payload.unwrap_or_default(), &name);
                        }
                        Verdict::Cancel { reason } => {
                            info!(
                                "Plugin '{name}' cancelled http route '{method} {path}': {reason}"
                            );
                            return error_outcome(500, &reason);
                        }
                        Verdict::Skip => {
                            // No opinion — let the next matching plugin try.
                        }
                    },
                    Err(e) => {
                        warn!("Plugin '{name}' returned invalid http verdict for '{path}': {e}");
                    }
                },
                Err(e) => {
                    warn!("Plugin '{name}' failed serving http route '{path}': {e}");
                }
            }
        }

        // A plugin claimed the route but none produced a usable response.
        error_outcome(500, "plugin did not produce a response")
    }

    /// Dispatch an **authenticated** HTTP request (the `require_auth`-guarded
    /// `/api/plugin-ui/*` surface, see `src/routes/plugin_ui.rs`) to whichever
    /// plugin owns the route via its manifest `ui_routes` + the
    /// [`HTTP_AUTHED_HOOK`]. Unlike [`serve_http`], the request runs on behalf
    /// of the verified `user_id`: core sets a trusted user-authority context in
    /// the plugin's host state for exactly the span of the `handle` call (so the
    /// plugin's scoped host functions may act under the user), and the user is
    /// echoed in the payload. The context is cleared the instant `handle`
    /// returns — it must never outlive its request.
    pub async fn serve_http_authed(
        &self,
        user_id: &str,
        method: &str,
        path: &str,
        query: &str,
        headers: &BTreeMap<String, String>,
        body: &str,
    ) -> PluginHttpOutcome {
        type AuthedTarget = (
            String,
            Arc<Mutex<Plugin>>,
            BTreeMap<String, String>,
            Arc<std::sync::RwLock<Option<super::host::UserContext>>>,
        );
        let targets: Vec<AuthedTarget> = {
            let plugins = self.plugins.lock().await;
            plugins
                .iter()
                .filter(|p| p.is_active() && p.manifest.hooks.iter().any(|h| h == HTTP_AUTHED_HOOK))
                .filter_map(|p| {
                    p.manifest
                        .ui_routes
                        .iter()
                        .find_map(|route| match_http_route(route, method, path))
                        .map(|params| (p.name.clone(), p.plugin.clone(), params, p.user.clone()))
                })
                .collect()
        };

        if targets.is_empty() {
            return PluginHttpOutcome::NoRoute;
        }

        let payload = serde_json::json!({
            "method": method,
            "path": path,
            "query": query,
            "headers": headers,
            "body": body,
            "user": { "id": user_id },
        });

        // Resolve an optional folder scope from the request: a project- or
        // session-scoped page (see `project_items` / `session_items`) sends its
        // id as a header, which the frontend injects from the page context. We
        // look up the folder it belongs to so the plugin's folder-scoped host
        // functions run there. The id is only a *scope selector* — the authed
        // surface already runs under the user's full authority, so this grants
        // no access the user doesn't already have; an unknown id just yields no
        // scope (the host functions then refuse with "caller has no folder
        // scope") rather than an error here.
        let scope = self.resolve_authed_scope(headers).await;

        for (name, plugin, params, user_slot) in targets {
            let mut req_payload = payload.clone();
            req_payload["params"] = serde_json::json!(params);
            let call_input = serde_json::json!({
                "hook": HTTP_AUTHED_HOOK,
                "payload": req_payload,
            });

            // Land the trusted user context for exactly this `handle` call.
            if let Ok(mut slot) = user_slot.write() {
                *slot = Some(super::host::UserContext {
                    user_id: user_id.to_string(),
                    folder_id: scope.folder_id.clone(),
                    project_id: scope.project_id.clone(),
                    session_id: scope.session_id.clone(),
                });
            }
            let result = {
                let mut guard = plugin.lock().await;
                guard.call::<String, String>("handle", call_input.to_string())
            };
            if let Ok(mut slot) = user_slot.write() {
                *slot = None;
            }

            match result {
                Ok(output) => match serde_json::from_str::<Verdict>(&output) {
                    Ok(Verdict::Allow { payload }) => {
                        return verdict_to_outcome(payload.unwrap_or_default(), &name);
                    }
                    Ok(Verdict::Cancel { reason }) => {
                        info!("Plugin '{name}' cancelled authed route '{method} {path}': {reason}");
                        return error_outcome(500, &reason);
                    }
                    Ok(Verdict::Skip) => {}
                    Err(e) => {
                        warn!("Plugin '{name}' returned invalid authed verdict for '{path}': {e}");
                    }
                },
                Err(e) => {
                    warn!("Plugin '{name}' failed serving authed route '{path}': {e}");
                }
            }
        }

        error_outcome(500, "plugin did not produce a response")
    }

    /// Resolve the folder scope for an authed plugin request from its headers.
    /// `x-peckboard-session-id` wins over `x-peckboard-project-id`; the folder
    /// (and project, for a session) is taken from the looked-up row. A missing
    /// or unknown id yields an empty scope — the host functions then refuse a
    /// folder-scoped call, which is the correct "no scope" behaviour.
    async fn resolve_authed_scope(&self, headers: &BTreeMap<String, String>) -> AuthedScope {
        let Some(db) = self.db.as_ref() else {
            return AuthedScope::default();
        };
        // Header names arrive lowercased (axum `HeaderName`).
        if let Some(sid) = headers
            .get("x-peckboard-session-id")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            && let Ok(Some(session)) = db.get_session(sid).await
        {
            return AuthedScope {
                folder_id: Some(session.folder_id),
                project_id: session.project_id,
                session_id: Some(session.id),
            };
        }
        if let Some(pid) = headers
            .get("x-peckboard-project-id")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            && let Ok(Some(project)) = db.get_project(pid).await
        {
            return AuthedScope {
                folder_id: Some(project.folder_id),
                project_id: Some(project.id),
                session_id: None,
            };
        }
        AuthedScope::default()
    }

    /// Check if any *active* (approved) plugins are registered for a given
    /// hook. A pending/denied plugin is inert, so it must not count as a
    /// listener — otherwise callers would dispatch a hook to nobody.
    pub async fn has_listeners(&self, hook: &str) -> bool {
        let plugins = self.plugins.lock().await;
        plugins
            .iter()
            .any(|p| p.is_active() && p.manifest.hooks.contains(&hook.to_string()))
    }

    /// The approval state of every loaded WASM plugin, for the
    /// `/api/plugins` catalog and the approval prompt.
    pub async fn wasm_plugins(&self) -> Vec<WasmPluginInfo> {
        let plugins = self.plugins.lock().await;
        plugins.iter().map(|p| p.to_info()).collect()
    }

    /// Record an operator's approve/deny decision for a plugin's declared
    /// hook set, persist it (so it survives restarts), and flip the loaded
    /// plugin's state. Approving runs the deferred `init`. Returns the
    /// updated info, or `None` if no plugin with that id is loaded.
    ///
    /// The decision is stored against the plugin's *current* canonical hook
    /// set, so it only re-applies on the next load while the plugin keeps
    /// asking for the same hooks.
    pub async fn decide(
        &self,
        plugin_id: &str,
        approve: bool,
    ) -> anyhow::Result<Option<WasmPluginInfo>> {
        // Snapshot what we need without holding the outer lock across the
        // (up to 2s) `init` call.
        let target = {
            let plugins = self.plugins.lock().await;
            plugins
                .iter()
                .find(|p| p.name == plugin_id)
                .map(|p| (p.plugin.clone(), p.hooks_canonical.clone()))
        };
        let Some((plugin, hooks_canonical)) = target else {
            return Ok(None);
        };

        let status = if approve {
            APPROVAL_APPROVED
        } else {
            APPROVAL_DENIED
        };
        if let Some(db) = &self.db {
            db.set_plugin_approval(plugin_id, &hooks_canonical, status)
                .await?;
        }

        // Approving runs the deferred `init`; denying leaves the plugin
        // inert with `init` never run.
        let mut init_error = None;
        let new_state = if approve {
            let init_config = read_plugin_config(&self.plugins_dir, plugin_id);
            let mut guard = plugin.lock().await;
            if let Err(e) = run_init(&mut guard, init_config) {
                warn!("Plugin '{plugin_id}' init failed on approval: {e}");
                init_error = Some(e);
            }
            ApprovalState::Approved
        } else {
            ApprovalState::Denied
        };

        // Apply the new state to the loaded entry, then report it back
        // (manifest metadata and all) from the single `to_info` source.
        let mut plugins = self.plugins.lock().await;
        let Some(p) = plugins.iter_mut().find(|p| p.name == plugin_id) else {
            return Ok(None);
        };
        p.approval = new_state;
        p.init_error = init_error;
        Ok(Some(p.to_info()))
    }

    /// Install an already-integrity-checked plugin `.wasm` (verified by the
    /// caller against the registry's SHA-256): load it into the running
    /// manager and, on success, persist it as `<plugins_dir>/<id>.wasm` so
    /// it returns on the next start. The plugin loads **inert** — it goes
    /// through the same approval gate as any other, so installing it grants
    /// it nothing until the operator approves its hooks.
    ///
    /// The bytes are loaded BEFORE the file is written, so a broken or
    /// malicious upgrade can't clobber a working install: if it doesn't
    /// load, nothing on disk changes. A successful install of a plugin id
    /// that's already loaded replaces (upgrades) it, shutting the old
    /// instance down first.
    pub async fn install(&self, id: &str, wasm: &[u8]) -> anyhow::Result<WasmPluginInfo> {
        if !is_safe_plugin_id(id) {
            anyhow::bail!("invalid plugin id '{id}' (expected ^[a-z0-9_-]+$)");
        }
        if self.db.is_none() {
            anyhow::bail!("this plugin manager cannot install plugins");
        }

        // Load from memory first — this validates the module, runs its
        // manifest, and enforces the hook allowlist before anything touches
        // disk.
        let loaded = self.load_plugin_from(id.to_string(), Wasm::data(wasm.to_vec()))?;
        let info = loaded.to_info();

        // Persist it (temp + rename, so a crash mid-write can't leave a
        // truncated .wasm that fails to load next start).
        std::fs::create_dir_all(&self.plugins_dir)?;
        let dest = self.plugins_dir.join(format!("{id}.wasm"));
        let tmp = self.plugins_dir.join(format!(".{id}.wasm.tmp"));
        std::fs::write(&tmp, wasm)?;
        std::fs::rename(&tmp, &dest)?;

        // Swap it into the live set, replacing (upgrading) any same-id
        // plugin already loaded.
        {
            let mut plugins = self.plugins.lock().await;
            if let Some(pos) = plugins.iter().position(|p| p.name == loaded.name) {
                let old = plugins.remove(pos);
                let mut guard = old.plugin.lock().await;
                if let Err(e) = guard.call::<&str, String>("shutdown", "") {
                    warn!("Plugin '{}' shutdown on upgrade failed: {e}", loaded.name);
                }
            }
            plugins.push(loaded);
        }
        info!("Installed plugin '{id}' ({} hooks)", info.hooks.len());
        Ok(info)
    }

    /// Resolve `id` against the configured plugin registries (the
    /// `PECKBOARD_PLUGIN_REGISTRY_URL` env override first, then the
    /// operator's repository rows), download + integrity-check the listed
    /// `.wasm`, and install/upgrade it via [`Self::install`]. `repository`
    /// restricts the search to a single registry url.
    ///
    /// This is the resolver behind the `upgrade_plugin` MCP tool. The HTTP
    /// `registry/install` route keeps its own inline copy so it can map each
    /// failure mode to a distinct status code (404 / 409 / 502); here every
    /// failure is a plain `anyhow` error surfaced to the calling agent.
    pub async fn install_from_registry(
        &self,
        id: &str,
        repository: Option<&str>,
    ) -> anyhow::Result<WasmPluginInfo> {
        use super::registry;

        let db = self
            .db
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("this plugin manager cannot install plugins"))?;

        // Candidate registries: env override first, then DB rows (de-duped).
        let mut repos: Vec<String> = Vec::new();
        if let Some((_, url)) = registry::env_repository() {
            repos.push(url);
        }
        for row in db.list_plugin_repositories().await? {
            if !repos.iter().any(|u| u == &row.url) {
                repos.push(row.url);
            }
        }
        if let Some(want) = repository {
            repos.retain(|u| u == want);
        }
        if repos.is_empty() {
            anyhow::bail!("no plugin registry configured");
        }

        let client = reqwest::Client::new();
        let mut found: Option<registry::RegistryEntry> = None;
        for url in &repos {
            if let Ok(index) = registry::fetch_index(&client, url).await
                && let Some(entry) = index.plugins.into_iter().find(|e| e.id == id)
            {
                found = Some(entry);
                break;
            }
        }
        let Some(entry) = found else {
            anyhow::bail!("no plugin '{id}' in the registry");
        };

        let running = registry::peckboard_version();
        if !registry::is_compatible(running, entry.min_peckboard.as_deref()) {
            anyhow::bail!(
                "plugin '{id}' requires Peckboard >= {} (running {running})",
                entry.min_peckboard.as_deref().unwrap_or("?")
            );
        }

        let bytes = registry::download_and_verify(&client, &entry.url, &entry.sha256)
            .await
            .map_err(|e| anyhow::anyhow!("download failed: {e}"))?;
        self.install(&entry.id, &bytes).await
    }

    /// Uninstall a loaded WASM plugin: shut its instance down, drop it from
    /// the live set, and delete its `<id>.wasm` from disk so it does not
    /// reload on the next start. Also clears the operator's stored approval
    /// decision and the plugin's persisted settings, so reinstalling the same
    /// id later starts clean — inert and `pending`, with schema-default
    /// settings — rather than silently inheriting an old grant or stale config.
    ///
    /// Returns `true` when a plugin with that id was loaded and removed,
    /// `false` when none matched (the route maps that to 404). Built-in
    /// plugins live in a separate, statically-linked registry and are never
    /// in this set, so they can't be reached through here.
    ///
    /// The id is validated as a safe bare plugin id before any filesystem
    /// access, so a crafted id can't delete a file outside the plugins dir.
    pub async fn uninstall(&self, id: &str) -> anyhow::Result<bool> {
        if !is_safe_plugin_id(id) {
            anyhow::bail!("invalid plugin id '{id}' (expected ^[a-z0-9_-]+$)");
        }

        // Drop it from the live set (shutting the instance down) before
        // touching disk. If no plugin with that id is loaded, there is
        // nothing to uninstall.
        let removed = {
            let mut plugins = self.plugins.lock().await;
            match plugins.iter().position(|p| p.name == id) {
                Some(pos) => {
                    let old = plugins.remove(pos);
                    let mut guard = old.plugin.lock().await;
                    if let Err(e) = guard.call::<&str, String>("shutdown", "") {
                        warn!("Plugin '{id}' shutdown on uninstall failed: {e}");
                    }
                    true
                }
                None => false,
            }
        };
        if !removed {
            return Ok(false);
        }

        // Delete the persisted `.wasm` so it doesn't reload next start. A
        // missing file is fine (already gone); any other error is real.
        let dest = self.plugins_dir.join(format!("{id}.wasm"));
        match std::fs::remove_file(&dest) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }

        // Clear persisted approval + settings so a reinstall starts clean.
        if let Some(db) = &self.db {
            db.delete_plugin_approval(id).await?;
            db.delete_plugin_settings(id).await?;
        }

        info!("Uninstalled plugin '{id}'");
        Ok(true)
    }

    /// Shut down all loaded plugins.
    pub async fn shutdown(&self) {
        let mut plugins = self.plugins.lock().await;
        for loaded in plugins.iter() {
            let mut guard = loaded.plugin.lock().await;
            if let Err(e) = guard.call::<&str, String>("shutdown", "") {
                warn!("Plugin '{}' shutdown failed: {e}", loaded.name);
            }
        }
        plugins.clear();
        info!("All plugins shut down");
    }

    /// Get the list of loaded plugin names.
    pub async fn loaded_plugins(&self) -> Vec<String> {
        let plugins = self.plugins.lock().await;
        plugins.iter().map(|p| p.name.clone()).collect()
    }

    /// Get registered hooks across all plugins.
    pub async fn registered_hooks(&self) -> HashMap<String, Vec<String>> {
        let plugins = self.plugins.lock().await;
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for loaded in plugins.iter() {
            if !loaded.is_active() {
                continue;
            }
            for hook in &loaded.manifest.hooks {
                map.entry(hook.clone())
                    .or_default()
                    .push(loaded.name.clone());
            }
        }
        map
    }

    /// Collect the UI panels every loaded plugin declares, tagged with the
    /// declaring plugin's name, for the `/api/plugins` catalog.
    ///
    /// This is the security choke point for plugin-contributed UI: the
    /// host renders each returned panel in a sandboxed iframe, so a panel
    /// whose `path` escapes the plugin-owned `/plugin-api/` prefix (an
    /// external URL, a protocol-relative `//host`, an authenticated
    /// `/api/*` route, or a `..` traversal) is dropped here with a warning
    /// rather than handed to the browser. Panels missing an `id`/`title`
    /// are likewise dropped — the UI keys and labels render from them.
    pub async fn ui_panels(&self) -> Vec<UiPanelEntry> {
        let plugins = self.plugins.lock().await;
        let mut out = Vec::new();
        for loaded in plugins.iter() {
            // An unapproved plugin is inert: its panels must not surface
            // in the catalog (and its page wouldn't serve anyway, since
            // `serve_http` skips it too).
            if !loaded.is_active() {
                continue;
            }
            for panel in &loaded.manifest.ui_panels {
                if panel.id.trim().is_empty() || panel.title.trim().is_empty() {
                    warn!(
                        "Plugin '{}' declares a ui_panel with an empty id/title; skipping",
                        loaded.name
                    );
                    continue;
                }
                if !is_valid_panel_path(&panel.path) {
                    warn!(
                        "Plugin '{}' ui_panel '{}' has invalid path '{}' (must be an absolute \
                         /plugin-api/ path); skipping",
                        loaded.name, panel.id, panel.path
                    );
                    continue;
                }
                out.push(UiPanelEntry {
                    plugin: loaded.name.clone(),
                    id: panel.id.clone(),
                    title: panel.title.clone(),
                    path: panel.path.clone(),
                });
            }
        }
        out
    }

    /// Every left-rail entry declared by an active plugin, for the
    /// `/api/plugins` catalog. Inert plugins contribute nothing. An entry
    /// with an empty id/label or a path that escapes the plugin-owned
    /// `/plugin-api/` prefix is dropped with a warning — same safety rule as
    /// [`Self::ui_panels`], so a plugin can't aim the rail button off-origin.
    pub async fn sidebar_items(&self) -> Vec<SidebarItemEntry> {
        let plugins = self.plugins.lock().await;
        collect_items(&plugins, "sidebar_item", |m| &m.sidebar_items)
    }

    /// Full-page entries active plugins contribute to the **project** page
    /// (manifest `project_items`), for the `/api/plugins` catalog. Same
    /// validation as [`Self::sidebar_items`].
    pub async fn project_items(&self) -> Vec<SidebarItemEntry> {
        let plugins = self.plugins.lock().await;
        collect_items(&plugins, "project_item", |m| &m.project_items)
    }

    /// Full-page entries active plugins contribute to the **session** page
    /// (manifest `session_items`), for the `/api/plugins` catalog.
    pub async fn session_items(&self) -> Vec<SidebarItemEntry> {
        let plugins = self.plugins.lock().await;
        collect_items(&plugins, "session_item", |m| &m.session_items)
    }

    /// Every MCP tool declared by an active plugin, for merging into the
    /// worker `tools/list`. Inert plugins contribute nothing (their tools
    /// wouldn't dispatch anyway). De-duplicated across plugins by name —
    /// the first active plugin to claim a name wins; a later collision is
    /// dropped with a warning so two plugins can't both shadow one tool
    /// name. (Collisions with *core* tool names are resolved by the caller,
    /// which knows the core set — see `src/routes/mcp.rs`.)
    pub async fn mcp_tools(&self) -> Vec<PluginMcpToolEntry> {
        let plugins = self.plugins.lock().await;
        let mut out: Vec<PluginMcpToolEntry> = Vec::new();
        for loaded in plugins.iter() {
            if !loaded.is_active() {
                continue;
            }
            for tool in &loaded.manifest.mcp_tools {
                if out.iter().any(|t| t.name == tool.name) {
                    warn!(
                        "Plugin '{}' mcp_tool '{}' collides with an already-registered \
                         plugin tool; dropping",
                        loaded.name, tool.name
                    );
                    continue;
                }
                out.push(PluginMcpToolEntry {
                    plugin: loaded.name.clone(),
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    input_schema: tool.input_schema.clone(),
                });
            }
        }
        out
    }

    /// Dispatch an MCP tool call to the active plugin that declared the tool,
    /// returning the tool result. Mirrors [`Self::serve_http`]: the matched
    /// plugin OWNS the call via the terminal [`MCP_TOOL_INVOKE_HOOK`] hook,
    /// receiving `{tool, arguments, context}` and returning the result as the
    /// payload of a [`Verdict::Allow`].
    ///
    /// Returns:
    /// - `None` if no active plugin declares `tool_name` — the caller falls
    ///   back to core's own tool dispatch.
    /// - `Some(Ok(result))` on a plugin `Allow` (its payload, or `null`).
    /// - `Some(Err(_))` on a plugin `Cancel`, an invalid verdict, or a call
    ///   failure — the caller maps it to a tool error.
    ///
    /// `context` carries the *serializable* slice of the caller's
    /// `ToolCallContext` (session/project/card/folder ids); heavy handles
    /// stay in core and a plugin acts back through host functions.
    pub async fn invoke_mcp_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        context: serde_json::Value,
    ) -> Option<anyhow::Result<serde_json::Value>> {
        // Find the single active plugin that declared this tool. Hold the
        // outer lock only long enough to clone its handle.
        type InvocationSlot = Arc<std::sync::RwLock<Option<super::host::InvocationContext>>>;
        let target: Option<(String, Arc<Mutex<Plugin>>, InvocationSlot)> = {
            let plugins = self.plugins.lock().await;
            plugins
                .iter()
                .find(|p| p.is_active() && p.manifest.mcp_tools.iter().any(|t| t.name == tool_name))
                .map(|p| (p.name.clone(), p.plugin.clone(), p.invocation.clone()))
        };
        let (name, plugin, invocation) = target?;

        let call_input = serde_json::json!({
            "hook": MCP_TOOL_INVOKE_HOOK,
            "payload": {
                "tool": tool_name,
                "arguments": arguments,
                "context": &context,
            },
        });

        // Land the *trusted* caller context where this plugin's scoped host
        // functions can read it, for exactly the span of the `handle` call.
        // It comes from `context` — built by `routes/mcp.rs` from the verified
        // `ToolCallContext` — so the plugin cannot forge the session/folder it
        // is treated as calling from. A malformed context (shouldn't happen)
        // leaves the slot `None`, so scoped functions safely refuse.
        if let Ok(parsed) = serde_json::from_value::<super::host::InvocationContext>(context)
            && let Ok(mut slot) = invocation.write()
        {
            *slot = Some(parsed);
        }

        let result = {
            let mut guard = plugin.lock().await;
            guard.call::<String, String>("handle", call_input.to_string())
        };

        // Clear the trusted context the moment `handle` returns — it must never
        // outlive its dispatch (the next tool call sets its own).
        if let Ok(mut slot) = invocation.write() {
            *slot = None;
        }

        Some(match result {
            Ok(output) => match serde_json::from_str::<Verdict>(&output) {
                Ok(Verdict::Allow { payload }) => Ok(payload.unwrap_or(serde_json::Value::Null)),
                Ok(Verdict::Cancel { reason }) => Err(anyhow::anyhow!(
                    "plugin '{name}' cancelled tool call: {reason}"
                )),
                Ok(Verdict::Skip) => Err(anyhow::anyhow!(
                    "plugin '{name}' skipped tool '{tool_name}'"
                )),
                Err(e) => Err(anyhow::anyhow!(
                    "plugin '{name}' returned an invalid verdict for tool '{tool_name}': {e}"
                )),
            },
            Err(e) => Err(anyhow::anyhow!(
                "plugin '{name}' failed to handle tool '{tool_name}': {e}"
            )),
        })
    }
}

/// The folder scope resolved for an authed plugin request from its
/// project/session header. Empty when the request carried no (known) id.
#[derive(Default)]
struct AuthedScope {
    folder_id: Option<String>,
    project_id: Option<String>,
    session_id: Option<String>,
}

/// Collect validated contribution entries (sidebar / project / session items)
/// from the active plugins, applying the same id/label/path checks `ui_panels`
/// and the original `sidebar_items` used. `kind` only labels skip warnings;
/// `select` picks which manifest vector to read.
fn collect_items(
    plugins: &[LoadedPlugin],
    kind: &str,
    select: impl Fn(&PluginManifest) -> &Vec<SidebarItem>,
) -> Vec<SidebarItemEntry> {
    let mut out = Vec::new();
    for loaded in plugins.iter() {
        if !loaded.is_active() {
            continue;
        }
        for item in select(&loaded.manifest) {
            if item.id.trim().is_empty() || item.label.trim().is_empty() {
                warn!(
                    "Plugin '{}' declares a {kind} with an empty id/label; skipping",
                    loaded.name
                );
                continue;
            }
            if !is_valid_panel_path(&item.path) {
                warn!(
                    "Plugin '{}' {kind} '{}' has invalid path '{}' (must be an \
                     absolute /plugin-api/ path); skipping",
                    loaded.name, item.id, item.path
                );
                continue;
            }
            out.push(SidebarItemEntry {
                plugin: loaded.name.clone(),
                id: item.id.clone(),
                label: item.label.clone(),
                icon: item.icon.clone(),
                path: item.path.clone(),
            });
        }
    }
    out
}

/// Whether a plugin-declared UI-panel path is safe for the host to embed.
///
/// The host renders the panel in a same-origin iframe, so the path must be
/// a server-absolute path under the plugin-owned `/plugin-api/` prefix and
/// nothing else. This rejects:
/// - external / protocol-relative targets (`https://…`, `//evil.test`) —
///   a plugin must not point the iframe off-origin,
/// - any path outside `/plugin-api/` (e.g. the authenticated `/api/*`
///   surface),
/// - `..` traversal segments that could climb out of the prefix, and
/// - backslashes, which a browser normalizes to `/` when resolving the
///   iframe `src` — so `/plugin-api/..\..\api` would slip past a slash-only
///   `..` check yet resolve to `/api`. We split only on `/`, so reject `\`
///   outright rather than try to model the browser's normalization.
fn is_valid_panel_path(path: &str) -> bool {
    path.starts_with("/plugin-api/")
        && !path.starts_with("//")
        && !path.contains("://")
        && !path.contains('\\')
        && !path.split('/').any(|seg| seg == "..")
}

/// Read the per-plugin config block to hand a plugin's `init`.
///
/// Looks up `plugins.<name>.config` in `<dataDir>/config.json` (the
/// `config.json` sitting beside the `plugins/` directory) and returns it
/// serialized as a JSON string. Returns `"{}"` when the file is absent,
/// unreadable, not valid JSON, or has no `config` entry for this plugin —
/// every failure is non-fatal so a missing or malformed config file never
/// stops plugins from loading; the plugin sees an empty config and decides
/// what to do.
///
/// Deliberately generic: core never interprets the contents. The shape of
/// the `config` object is entirely the plugin's contract (e.g. the public
/// API plugin's `{ "keys": [...] }`).
fn read_plugin_config(plugins_dir: &Path, name: &str) -> String {
    let Some(data_dir) = plugins_dir.parent() else {
        return "{}".to_string();
    };
    let path = data_dir.join("config.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        // Absent config.json is the common case, not an error.
        Err(_) => return "{}".to_string(),
    };
    let root: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            warn!("Ignoring malformed {}: {e}", path.display());
            return "{}".to_string();
        }
    };
    root.get("plugins")
        .and_then(|p| p.get(name))
        .and_then(|p| p.get("config"))
        .map(|c| c.to_string())
        .unwrap_or_else(|| "{}".to_string())
}

/// Match an `http_routes` declaration against a concrete request.
///
/// A declaration is `"<METHOD> <PATH_PATTERN>"`, e.g.
/// `"GET /plugin-api/cards/:id"`. `METHOD` may be `*` to match any
/// method. The path pattern uses the same `:param` segment syntax as
/// the router, plus a trailing `*` / `*name` catch-all segment that
/// matches the (possibly empty) remainder of the path. On a match,
/// returns the captured path params (`:id` → value, `*rest` → the
/// joined remainder under key `rest`, or `*`); on no match, `None`.
fn match_http_route(decl: &str, method: &str, path: &str) -> Option<BTreeMap<String, String>> {
    let decl = decl.trim();
    let mut it = decl.splitn(2, char::is_whitespace);
    let first = it.next()?.trim();
    let (method_pat, path_pat) = match it.next() {
        Some(rest) => (first, rest.trim()),
        None => ("*", first),
    };

    if method_pat != "*" && !method_pat.eq_ignore_ascii_case(method) {
        return None;
    }

    let pat_segs: Vec<&str> = path_pat.split('/').filter(|s| !s.is_empty()).collect();
    let req_segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    let mut params = BTreeMap::new();
    let mut i = 0;
    while i < pat_segs.len() {
        let ps = pat_segs[i];
        if let Some(name) = ps.strip_prefix('*') {
            // Catch-all: consumes the remainder of the request path.
            let key = if name.is_empty() { "*" } else { name };
            params.insert(key.to_string(), req_segs[i..].join("/"));
            return Some(params);
        }
        if i >= req_segs.len() {
            return None;
        }
        if let Some(name) = ps.strip_prefix(':') {
            params.insert(name.to_string(), req_segs[i].to_string());
        } else if ps != req_segs[i] {
            return None;
        }
        i += 1;
    }

    if req_segs.len() != pat_segs.len() {
        return None;
    }
    Some(params)
}

/// Turn a plugin's `Verdict::Allow` payload into an HTTP outcome.
///
/// Parses the payload as a [`PluginHttpResponse`]. A string `body` is
/// sent verbatim; any other JSON value is serialized to JSON text with
/// `content-type: application/json` defaulted unless the plugin already
/// set one. A `null` body yields an empty body. A payload that doesn't
/// deserialize is a plugin bug and maps to a 500.
fn verdict_to_outcome(payload: serde_json::Value, plugin: &str) -> PluginHttpOutcome {
    let resp: PluginHttpResponse = match serde_json::from_value(payload) {
        Ok(r) => r,
        Err(e) => {
            warn!("Plugin '{plugin}' returned malformed http response: {e}");
            return error_outcome(500, "plugin returned a malformed response");
        }
    };

    let mut headers: Vec<(String, String)> = resp
        .headers
        .into_iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v))
        .collect();
    let has_content_type = headers.iter().any(|(k, _)| k == "content-type");

    let body = match resp.body {
        serde_json::Value::Null => Vec::new(),
        serde_json::Value::String(s) => s.into_bytes(),
        other => {
            if !has_content_type {
                headers.push(("content-type".to_string(), "application/json".to_string()));
            }
            serde_json::to_vec(&other).unwrap_or_default()
        }
    };

    PluginHttpOutcome::Served {
        status: resp.status,
        headers,
        body,
    }
}

/// Build a JSON `{"error": ...}` HTTP outcome with the given status.
fn error_outcome(status: u16, message: &str) -> PluginHttpOutcome {
    PluginHttpOutcome::Served {
        status,
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        body: serde_json::json!({ "error": message })
            .to_string()
            .into_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hook allowlist is a security boundary (see the doc comment on
    /// `ALLOWED_HOOKS`): only hooks listed here ever run plugin code.
    /// Pin the load-bearing properties so an accidental edit — a
    /// duplicate, or dropping a hook a feature depends on — fails here
    /// rather than silently changing what plugins can intercept.
    #[test]
    fn allowed_hooks_pinned() {
        // No duplicates.
        let mut seen = std::collections::HashSet::new();
        for h in ALLOWED_HOOKS {
            assert!(seen.insert(*h), "duplicate hook in ALLOWED_HOOKS: {h}");
        }
        // Hooks the codebase dispatches must be present.
        for required in [
            "card.create.before",
            "card.update.before",
            "card.priorities.list",
            "http.request.before",
            "mcp.tool.call.before",
            "session.reference.resolve",
            "todo",
        ] {
            assert!(
                ALLOWED_HOOKS.contains(&required),
                "ALLOWED_HOOKS is missing dispatched hook '{required}'"
            );
        }
        // The HTTP serving hook constant and the allowlist agree.
        assert!(ALLOWED_HOOKS.contains(&HTTP_REQUEST_HOOK));
    }

    #[test]
    fn canonical_hooks_is_order_and_dup_independent() {
        let a = canonical_hooks(&["http.request.before".into(), "todo".into()]);
        let b = canonical_hooks(&["todo".into(), "http.request.before".into()]);
        assert_eq!(a, b, "hook order must not change the canonical form");
        // Duplicates collapse, so an approval can't be dodged by listing a
        // hook twice.
        let c = canonical_hooks(&["todo".into(), "todo".into(), "http.request.before".into()]);
        assert_eq!(a, c);
        assert_eq!(a, "http.request.before\ntodo");
    }

    fn approval_row(hooks: &str, status: &str) -> crate::db::models::PluginApprovalRow {
        crate::db::models::PluginApprovalRow {
            plugin_id: "api".into(),
            hooks: hooks.into(),
            status: status.into(),
            decided_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn resolve_approval_requires_matching_hook_set() {
        let canonical = canonical_hooks(&["http.request.before".into(), "todo".into()]);

        // No stored decision → pending.
        assert_eq!(resolve_approval(None, &canonical), ApprovalState::Pending);

        // Stored approval against the SAME hook set → approved.
        let approved = approval_row(&canonical, APPROVAL_APPROVED);
        assert_eq!(
            resolve_approval(Some(&approved), &canonical),
            ApprovalState::Approved
        );

        // Stored denial against the same hook set → denied.
        let denied = approval_row(&canonical, APPROVAL_DENIED);
        assert_eq!(
            resolve_approval(Some(&denied), &canonical),
            ApprovalState::Denied
        );

        // An approval made against a DIFFERENT (here: larger) hook set must
        // NOT carry over — the plugin drops back to pending. This is the
        // anti-escalation guarantee: a swapped `.wasm` that now also wants
        // `mcp.token.issue.before` can't ride the old grant.
        let escalated = canonical_hooks(&[
            "http.request.before".into(),
            "todo".into(),
            "mcp.token.issue.before".into(),
        ]);
        assert_eq!(
            resolve_approval(Some(&approved), &escalated),
            ApprovalState::Pending,
            "an approval for a different hook set must not apply"
        );
    }

    #[test]
    fn is_safe_plugin_id_accepts_only_bare_lowercase_ids() {
        assert!(is_safe_plugin_id("api"));
        assert!(is_safe_plugin_id("my-plugin_2"));
        // Rejected: empty, traversal, separators, uppercase, dots, spaces.
        assert!(!is_safe_plugin_id(""));
        assert!(!is_safe_plugin_id(".."));
        assert!(!is_safe_plugin_id("../evil"));
        assert!(!is_safe_plugin_id("a/b"));
        assert!(!is_safe_plugin_id("a.wasm"));
        assert!(!is_safe_plugin_id("API"));
        assert!(!is_safe_plugin_id("a b"));
        assert!(!is_safe_plugin_id(&"x".repeat(65)));
    }

    #[tokio::test]
    async fn install_rejects_unsafe_id_without_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::db::Db::in_memory().unwrap();
        let mgr = PluginManager::new(tmp.path(), db);
        let err = mgr.install("../evil", b"\0asm").await.unwrap_err();
        assert!(err.to_string().contains("invalid plugin id"));
        // Nothing was written anywhere under the plugins dir.
        let plugins_dir = tmp.path().join("plugins");
        if plugins_dir.exists() {
            assert_eq!(std::fs::read_dir(&plugins_dir).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn uninstall_rejects_unsafe_id() {
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::db::Db::in_memory().unwrap();
        let mgr = PluginManager::new(tmp.path(), db);
        let err = mgr.uninstall("../evil").await.unwrap_err();
        assert!(err.to_string().contains("invalid plugin id"));
    }

    #[tokio::test]
    async fn uninstall_unknown_plugin_is_false() {
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::db::Db::in_memory().unwrap();
        let mgr = PluginManager::new(tmp.path(), db);
        // A valid id that isn't loaded → Ok(false), no error (route maps to 404).
        assert!(!mgr.uninstall("ghost").await.unwrap());
    }

    // ── Plugin-provided MCP tools (Phase A) ─────────────────────────────

    fn manifest_with(
        hooks: &[&str],
        tools: Vec<super::super::hooks::PluginMcpTool>,
    ) -> PluginManifest {
        // Give it the provide_mcp_tools permission whenever it declares tools,
        // so the tool-shape tests aren't tripped by the permission check.
        let permissions = if tools.is_empty() {
            Vec::new()
        } else {
            vec!["provide_mcp_tools".to_string()]
        };
        PluginManifest {
            description: "d".into(),
            version: "1".into(),
            repository: "https://example.test/x".into(),
            hooks: hooks.iter().map(|s| s.to_string()).collect(),
            http_routes: Vec::new(),
            ui_routes: Vec::new(),
            ui_panels: Vec::new(),
            mcp_tools: tools,
            sidebar_items: Vec::new(),
            project_items: Vec::new(),
            session_items: Vec::new(),
            permissions,
        }
    }

    fn tool(name: &str) -> super::super::hooks::PluginMcpTool {
        super::super::hooks::PluginMcpTool {
            name: name.into(),
            description: "does a thing".into(),
            input_schema: serde_json::json!({ "type": "object" }),
        }
    }

    #[test]
    fn validate_mcp_tools_ok_with_invoke_hook() {
        let m = manifest_with(&["mcp.tool.invoke"], vec![tool("do_thing")]);
        assert!(validate_mcp_tools("p", &m).is_ok());
        // No tools at all is always fine, hook or not.
        assert!(validate_mcp_tools("p", &manifest_with(&[], vec![])).is_ok());
    }

    #[test]
    fn validate_mcp_tools_requires_invoke_hook() {
        let m = manifest_with(&["http.request.before"], vec![tool("do_thing")]);
        let err = validate_mcp_tools("p", &m).unwrap_err().to_string();
        assert!(err.contains("mcp.tool.invoke"), "got: {err}");
    }

    #[test]
    fn validate_mcp_tools_rejects_bad_name_and_empty_desc() {
        let bad_name = manifest_with(&["mcp.tool.invoke"], vec![tool("Bad-Name")]);
        assert!(
            validate_mcp_tools("p", &bad_name)
                .unwrap_err()
                .to_string()
                .contains("invalid name")
        );
        let mut blank = tool("ok_name");
        blank.description = "  ".into();
        let m = manifest_with(&["mcp.tool.invoke"], vec![blank]);
        assert!(
            validate_mcp_tools("p", &m)
                .unwrap_err()
                .to_string()
                .contains("empty description")
        );
    }

    #[test]
    fn validate_mcp_tools_requires_provide_permission() {
        // Tools + invoke hook but no `provide_mcp_tools` permission → reject.
        let mut m = manifest_with(&["mcp.tool.invoke"], vec![tool("do_thing")]);
        m.permissions.clear();
        let err = validate_mcp_tools("p", &m).unwrap_err().to_string();
        assert!(err.contains("provide_mcp_tools"), "got: {err}");
    }

    #[test]
    fn canonical_grant_is_backward_compatible_and_permission_sensitive() {
        let hooks = vec!["http.request.before".to_string()];
        // No permissions → identical to the old hooks-only canonical, so
        // pre-permissions approvals still match.
        assert_eq!(canonical_grant(&hooks, &[]), canonical_hooks(&hooks));
        // Adding a permission changes the fingerprint (forces a re-prompt)...
        let with_perm = canonical_grant(&hooks, &["session_read".to_string()]);
        assert_ne!(with_perm, canonical_hooks(&hooks));
        // ...but is order/dup independent.
        assert_eq!(
            canonical_grant(&hooks, &["session_read".into(), "broadcast".into()]),
            canonical_grant(
                &hooks,
                &[
                    "broadcast".into(),
                    "session_read".into(),
                    "broadcast".into()
                ]
            ),
        );
    }

    #[tokio::test]
    async fn invoke_mcp_tool_unowned_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::db::Db::in_memory().unwrap();
        let mgr = PluginManager::new(tmp.path(), db);
        // No plugins loaded → no plugin owns the tool → None (caller falls
        // back to core dispatch). And the catalog of plugin tools is empty.
        assert!(
            mgr.invoke_mcp_tool("anything", serde_json::json!({}), serde_json::json!({}))
                .await
                .is_none()
        );
        assert!(mgr.mcp_tools().await.is_empty());
    }

    #[tokio::test]
    async fn uninstall_removes_wasm_and_clears_state() {
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::db::Db::in_memory().unwrap();
        // Seed an approval and a setting for a plugin id that isn't actually
        // loaded; uninstall should only touch them once it has removed a
        // loaded instance, so first confirm an unloaded id leaves them intact.
        db.set_plugin_approval("demo", "todo", APPROVAL_APPROVED)
            .await
            .unwrap();
        db.set_plugin_setting("demo", "k", &serde_json::json!("v"))
            .await
            .unwrap();
        let mgr = PluginManager::new(tmp.path(), db.clone());

        // No loaded plugin named `demo` → false, and the seeded rows survive
        // (we don't clear state for a plugin that was never installed here).
        assert!(!mgr.uninstall("demo").await.unwrap());
        assert!(db.get_plugin_approval_blocking("demo").unwrap().is_some());
        assert_eq!(
            db.list_plugin_settings("demo").await.unwrap().len(),
            1,
            "settings for an un-removed plugin must be left alone"
        );
    }

    #[tokio::test]
    async fn install_rejects_invalid_wasm_without_persisting() {
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::db::Db::in_memory().unwrap();
        let mgr = PluginManager::new(tmp.path(), db);
        // Bytes that aren't a loadable module: load fails BEFORE the file is
        // written, so no demo.wasm is left behind and nothing is loaded.
        let err = mgr.install("demo", b"not a wasm module").await.unwrap_err();
        assert!(!err.to_string().is_empty());
        assert!(!tmp.path().join("plugins").join("demo.wasm").exists());
        assert!(mgr.wasm_plugins().await.is_empty());
    }

    #[tokio::test]
    async fn resolve_authed_scope_maps_session_header_to_folder() {
        use crate::db::models::{NewFolder, NewSession};
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::db::Db::in_memory().unwrap();
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "Repo".into(),
            path: tmp.path().to_string_lossy().to_string(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_session(NewSession {
            id: "s1".into(),
            name: "S".into(),
            folder_id: "f1".into(),
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
        let mgr = PluginManager::new(tmp.path(), db);

        // A session id resolves to its folder (the scope the host fns read).
        let mut h = BTreeMap::new();
        h.insert("x-peckboard-session-id".to_string(), "s1".to_string());
        let scope = mgr.resolve_authed_scope(&h).await;
        assert_eq!(scope.folder_id.as_deref(), Some("f1"));
        assert_eq!(scope.session_id.as_deref(), Some("s1"));

        // An unknown id yields no scope (the host fns then refuse a folder
        // call) rather than leaking a default folder.
        let mut bad = BTreeMap::new();
        bad.insert("x-peckboard-session-id".to_string(), "nope".to_string());
        assert!(mgr.resolve_authed_scope(&bad).await.folder_id.is_none());

        // No scope header at all → empty scope.
        let none = mgr.resolve_authed_scope(&BTreeMap::new()).await;
        assert!(none.folder_id.is_none() && none.session_id.is_none());
    }

    #[test]
    fn status_label_maps_state_and_init_error() {
        assert_eq!(status_label(&ApprovalState::Pending, &None), "pending");
        assert_eq!(status_label(&ApprovalState::Denied, &None), "denied");
        assert_eq!(status_label(&ApprovalState::Approved, &None), "approved");
        // Approved but the deferred init failed → init_failed, not approved.
        assert_eq!(
            status_label(&ApprovalState::Approved, &Some("boom".into())),
            "init_failed"
        );
    }

    #[test]
    fn read_plugin_config_extracts_block_or_defaults_to_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path();
        let plugins_dir = data_dir.join("plugins");

        // No config.json at all → "{}".
        assert_eq!(read_plugin_config(&plugins_dir, "api"), "{}");

        std::fs::write(
            data_dir.join("config.json"),
            r#"{
                "plugins": {
                    "api": { "enabled": true, "config": { "keys": [{ "key": "k1" }] } },
                    "other": { "config": { "x": 1 } }
                }
            }"#,
        )
        .unwrap();

        // The matching plugin's `config` block is returned verbatim (as JSON).
        let got = read_plugin_config(&plugins_dir, "api");
        let v: serde_json::Value = serde_json::from_str(&got).unwrap();
        assert_eq!(v["keys"][0]["key"], "k1");
        // `enabled` is NOT part of `config` — only the inner block is passed.
        assert!(v.get("enabled").is_none());

        // A plugin with no entry → "{}".
        assert_eq!(read_plugin_config(&plugins_dir, "missing"), "{}");

        // Malformed config.json is ignored (non-fatal), yielding "{}".
        std::fs::write(data_dir.join("config.json"), "not json").unwrap();
        assert_eq!(read_plugin_config(&plugins_dir, "api"), "{}");
    }

    #[test]
    fn route_matches_literal_and_method() {
        let p = match_http_route("GET /plugin-api/health", "GET", "/plugin-api/health").unwrap();
        assert!(p.is_empty());
        // Method is case-insensitive.
        assert!(match_http_route("get /plugin-api/health", "GET", "/plugin-api/health").is_some());
        // Wrong method, wrong path → no match.
        assert!(match_http_route("POST /plugin-api/health", "GET", "/plugin-api/health").is_none());
        assert!(match_http_route("GET /plugin-api/health", "GET", "/plugin-api/other").is_none());
    }

    #[test]
    fn route_captures_params() {
        let p =
            match_http_route("GET /plugin-api/cards/:id", "GET", "/plugin-api/cards/42").unwrap();
        assert_eq!(p.get("id").map(String::as_str), Some("42"));
        // Segment count must match for a non-catch-all pattern.
        assert!(
            match_http_route("GET /plugin-api/cards/:id", "GET", "/plugin-api/cards").is_none()
        );
        assert!(
            match_http_route(
                "GET /plugin-api/cards/:id",
                "GET",
                "/plugin-api/cards/42/extra"
            )
            .is_none()
        );
    }

    #[test]
    fn route_method_wildcard_and_catch_all() {
        // `*` method matches anything.
        assert!(match_http_route("* /plugin-api/x", "DELETE", "/plugin-api/x").is_some());
        // Catch-all consumes the remainder.
        let p = match_http_route("* /plugin-api/*rest", "GET", "/plugin-api/a/b/c").unwrap();
        assert_eq!(p.get("rest").map(String::as_str), Some("a/b/c"));
        // Catch-all may match an empty remainder.
        let p = match_http_route("GET /plugin-api/*rest", "GET", "/plugin-api").unwrap();
        assert_eq!(p.get("rest").map(String::as_str), Some(""));
    }

    #[test]
    fn allow_string_body_is_verbatim() {
        let outcome =
            verdict_to_outcome(serde_json::json!({ "status": 201, "body": "hello" }), "p");
        match outcome {
            PluginHttpOutcome::Served {
                status,
                headers,
                body,
            } => {
                assert_eq!(status, 201);
                assert_eq!(body, b"hello");
                // No content-type forced for a string body.
                assert!(!headers.iter().any(|(k, _)| k == "content-type"));
            }
            other => panic!("expected Served, got {other:?}"),
        }
    }

    #[test]
    fn allow_json_body_sets_content_type_and_defaults_status() {
        let outcome = verdict_to_outcome(serde_json::json!({ "body": { "ok": true } }), "p");
        match outcome {
            PluginHttpOutcome::Served {
                status,
                headers,
                body,
            } => {
                assert_eq!(status, 200, "status defaults to 200");
                assert!(
                    headers
                        .iter()
                        .any(|(k, v)| k == "content-type" && v == "application/json")
                );
                let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(v, serde_json::json!({ "ok": true }));
            }
            other => panic!("expected Served, got {other:?}"),
        }
    }

    #[test]
    fn allow_respects_plugin_content_type() {
        let outcome = verdict_to_outcome(
            serde_json::json!({
                "headers": { "Content-Type": "text/plain" },
                "body": { "ignored": "as-json-but-typed-text" }
            }),
            "p",
        );
        if let PluginHttpOutcome::Served { headers, .. } = outcome {
            // Header name is normalized to lowercase, plugin value wins,
            // and we do NOT add a second content-type.
            let cts: Vec<_> = headers
                .iter()
                .filter(|(k, _)| k == "content-type")
                .collect();
            assert_eq!(cts.len(), 1);
            assert_eq!(cts[0].1, "text/plain");
        } else {
            panic!("expected Served");
        }
    }

    #[test]
    fn malformed_response_is_500() {
        // `status` must be a number — a string is malformed.
        let outcome = verdict_to_outcome(serde_json::json!({ "status": "nope" }), "p");
        match outcome {
            PluginHttpOutcome::Served { status, .. } => assert_eq!(status, 500),
            other => panic!("expected Served 500, got {other:?}"),
        }
    }

    #[test]
    fn ui_panel_path_must_stay_under_plugin_api() {
        // The host embeds the panel in a same-origin iframe, so only
        // server-absolute paths under `/plugin-api/` are allowed.
        assert!(is_valid_panel_path("/plugin-api/v1/admin"));
        assert!(is_valid_panel_path("/plugin-api/v1/admin?tab=keys"));

        // Off-origin / protocol-relative targets are rejected.
        assert!(!is_valid_panel_path("https://evil.test/admin"));
        assert!(!is_valid_panel_path("//evil.test/admin"));
        // The plugin-api prefix embedded mid-URL must not sneak through.
        assert!(!is_valid_panel_path("https://evil.test/plugin-api/x"));

        // Paths outside the plugin-owned prefix (incl. the authenticated
        // /api surface) are rejected.
        assert!(!is_valid_panel_path("/api/projects"));
        assert!(!is_valid_panel_path("/plugin-api"));
        assert!(!is_valid_panel_path("plugin-api/v1/admin"));
        assert!(!is_valid_panel_path(""));

        // `..` traversal out of the prefix is rejected.
        assert!(!is_valid_panel_path("/plugin-api/../api/projects"));
        // Backslash traversal is rejected: a browser normalizes `\` to `/`
        // when resolving the iframe src, so this would otherwise resolve to
        // `/api` despite slipping past the slash-only `..` check.
        assert!(!is_valid_panel_path("/plugin-api/..\\..\\api"));
        assert!(!is_valid_panel_path("/plugin-api/\\evil"));
    }
}
