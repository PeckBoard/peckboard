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

/// A loaded plugin instance.
struct LoadedPlugin {
    name: String,
    manifest: PluginManifest,
    plugin: Plugin,
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

    /// Scan the plugins directory and load all .wasm files.
    pub async fn load_all(&self) -> anyhow::Result<()> {
        if !self.plugins_dir.exists() {
            std::fs::create_dir_all(&self.plugins_dir)?;
            info!("Created plugins directory at {}", self.plugins_dir.display());
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

        // Call init export.
        let init_result = plugin.call::<&str, String>("init", "{}");
        if let Err(e) = init_result {
            warn!("Plugin '{name}' init failed: {e}");
            return Err(anyhow::anyhow!("plugin init failed: {e}"));
        }

        Ok(LoadedPlugin {
            name,
            manifest: plugin_manifest,
            plugin,
        })
    }

    /// Dispatch a hook to all registered plugins.
    ///
    /// Plugins are called in load order. If any plugin cancels, dispatch stops.
    /// If a plugin modifies the payload, the modified version is passed to the next.
    pub async fn dispatch(
        &self,
        hook: &str,
        payload: serde_json::Value,
    ) -> HookResult {
        let mut plugins = self.plugins.lock().await;
        let mut current_payload = payload;

        for loaded in plugins.iter_mut() {
            if !loaded.manifest.hooks.contains(&hook.to_string()) {
                continue;
            }

            let call_input = serde_json::json!({
                "hook": hook,
                "payload": current_payload,
            });

            let result = loaded
                .plugin
                .call::<String, String>("handle".to_string(), call_input.to_string());

            match result {
                Ok(output) => match serde_json::from_str::<Verdict>(&output) {
                    Ok(Verdict::Allow { payload }) => {
                        if let Some(modified) = payload {
                            current_payload = modified;
                        }
                    }
                    Ok(Verdict::Cancel { reason }) => {
                        info!(
                            "Plugin '{}' cancelled hook '{}': {reason}",
                            loaded.name, hook
                        );
                        return HookResult::Cancelled {
                            plugin: loaded.name.clone(),
                            reason,
                        };
                    }
                    Ok(Verdict::Skip) => {
                        // No opinion, continue to next plugin
                    }
                    Err(e) => {
                        warn!(
                            "Plugin '{}' returned invalid verdict for hook '{}': {e}",
                            loaded.name, hook
                        );
                        // Treat parse errors as skip
                    }
                },
                Err(e) => {
                    warn!(
                        "Plugin '{}' failed on hook '{}': {e}",
                        loaded.name, hook
                    );
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
        for loaded in plugins.iter_mut() {
            if let Err(e) = loaded.plugin.call::<&str, String>("shutdown", "") {
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
