use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use extism::{Manifest as ExtismManifest, Plugin, Wasm};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use super::hooks::{HookResult, PluginManifest, Verdict};

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
}

/// Manages all loaded plugins and dispatches hook calls.
pub struct PluginManager {
    plugins: Arc<Mutex<Vec<LoadedPlugin>>>,
    plugins_dir: PathBuf,
}

impl PluginManager {
    /// Create a new plugin manager. Does not load plugins yet.
    pub fn new(data_dir: &Path) -> Self {
        PluginManager {
            plugins: Arc::new(Mutex::new(Vec::new())),
            plugins_dir: data_dir.join("plugins"),
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

        let mut plugin = Plugin::new(manifest, [], true)?;

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

        // Call init export.
        let init_result = plugin.call::<&str, String>("init", "{}");
        if let Err(e) = init_result {
            warn!("Plugin '{name}' init failed: {e}");
            return Err(anyhow::anyhow!("plugin init failed: {e}"));
        }

        Ok(LoadedPlugin {
            name,
            manifest: plugin_manifest,
            plugin: Arc::new(Mutex::new(plugin)),
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
                .filter(|p| p.manifest.hooks.contains(&hook.to_string()))
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

    /// Check if any plugins are registered for a given hook.
    pub async fn has_listeners(&self, hook: &str) -> bool {
        let plugins = self.plugins.lock().await;
        plugins
            .iter()
            .any(|p| p.manifest.hooks.contains(&hook.to_string()))
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
            for hook in &loaded.manifest.hooks {
                map.entry(hook.clone())
                    .or_default()
                    .push(loaded.name.clone());
            }
        }
        map
    }
}
