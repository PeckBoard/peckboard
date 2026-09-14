//! Shared helpers for integration tests that need first-party WASM providers.
//! Product AI backends are plugins — there is no compiled-in `AgentProvider`.

use std::path::Path;
use std::sync::Arc;

use peckboard::db::Db;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::registry::ProviderRegistry;

/// Extract first-party provider wasm into `data_dir/plugins`, auto-approve,
/// and register each plugin's `AgentProvider` adapter on `registry`.
pub async fn load_first_party_providers(
    data_dir: &Path,
    db: Db,
    registry: &Arc<ProviderRegistry>,
) -> Arc<PluginManager> {
    let plugins = Arc::new(PluginManager::new(data_dir, db));
    plugins
        .load_all()
        .await
        .expect("first-party provider plugins must load");
    plugins.set_provider_registry(registry);
    plugins.sync_plugin_providers().await;
    plugins
}
