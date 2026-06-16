use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use extism::{Manifest as ExtismManifest, Plugin, Wasm};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use serde::Serialize;

use super::hooks::{
    HTTP_REQUEST_HOOK, HookResult, PluginHttpOutcome, PluginHttpResponse, PluginManifest,
    UiPanelEntry, Verdict,
};
use crate::db::Db;
use crate::db::crud::{APPROVAL_APPROVED, APPROVAL_DENIED};

const MEMORY_LIMIT_PAGES: u32 = 2048; // 128 MB (64 KB per page)
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
    "session.reference.resolve",
    "todo",
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
    /// Every hook the plugin declares — what the operator is approving.
    pub hooks: Vec<String>,
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
}

impl PluginManager {
    /// Create a new plugin manager. Does not load plugins yet. The `db`
    /// handle backs the data-access host functions exposed to plugins.
    pub fn new(data_dir: &Path, db: Db) -> Self {
        PluginManager {
            plugins: Arc::new(Mutex::new(Vec::new())),
            plugins_dir: data_dir.join("plugins"),
            db: Some(db),
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

        let wasm = Wasm::file(path);
        let manifest = ExtismManifest::new([wasm])
            .with_timeout(CALL_TIMEOUT)
            .with_memory_max(MEMORY_LIMIT_PAGES);

        // Wire the data-access host functions into the plugin so it can read
        // and write Peckboard data through the sandbox. `empty()` managers have
        // no `Db` and never reach here, so they register nothing.
        let functions = match &self.db {
            // `name` is the plugin's id (its `.wasm` file stem), the same id
            // its `plugin_settings` rows are keyed by — so the self-storage
            // host functions stay scoped to this plugin's own namespace.
            Some(db) => super::host::host_functions(db, &name),
            None => Vec::new(),
        };
        let mut plugin = Plugin::new(manifest, functions, true)?;

        // Call manifest export to get hook declarations.
        let manifest_json = plugin.call::<&str, String>("manifest", "")?;
        let plugin_manifest: PluginManifest = serde_json::from_str(&manifest_json)?;

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

        // Resolve the operator's stored approval for this exact hook set.
        // A plugin is inert until approved (the user requires permission
        // for every hook), so a missing decision — or one made against a
        // different hook set — leaves it `Pending`, not active.
        let hooks_canonical = canonical_hooks(&plugin_manifest.hooks);
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
        plugins
            .iter()
            .map(|p| WasmPluginInfo {
                name: p.name.clone(),
                hooks: p.manifest.hooks.clone(),
                status: p.status_label(),
                error: p.init_error.clone(),
            })
            .collect()
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
            plugins.iter().find(|p| p.name == plugin_id).map(|p| {
                (
                    p.plugin.clone(),
                    p.hooks_canonical.clone(),
                    p.manifest.hooks.clone(),
                )
            })
        };
        let Some((plugin, hooks_canonical, hooks)) = target else {
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

        let status = status_label(&new_state, &init_error);

        // Apply the new state to the loaded entry.
        {
            let mut plugins = self.plugins.lock().await;
            if let Some(p) = plugins.iter_mut().find(|p| p.name == plugin_id) {
                p.approval = new_state;
                p.init_error = init_error.clone();
            }
        }

        Ok(Some(WasmPluginInfo {
            name: plugin_id.to_string(),
            hooks,
            status,
            error: init_error,
        }))
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
