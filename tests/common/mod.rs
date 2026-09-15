//! Shared helpers for integration tests that need first-party providers.
//! Product AI backends are plugins: first-party providers are trusted
//! crate plugins registered natively at boot, third-party providers load
//! as WASM. There is no hand-rolled `AgentProvider` in `src/provider/`.

use std::path::Path;
use std::sync::Arc;

use peckboard::db::Db;
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::registry::ProviderRegistry;

/// Mirror the boot sequence in `server.rs`: load WASM plugins from
/// `data_dir/plugins`, register the bundled crate plugins (which installs a
/// native `CrateAgentProvider` for each first-party provider id), then apply
/// any WASM-registered providers.
pub async fn load_first_party_providers(
    data_dir: &Path,
    db: Db,
    registry: &Arc<ProviderRegistry>,
) -> Arc<PluginManager> {
    let plugins = Arc::new(PluginManager::new(data_dir, db.clone()));
    plugins.load_all().await.expect("plugin manager must load");
    let builtin = Arc::new(BuiltinPluginRegistry::new());
    // Tests act like a fully-installed instance: every crate plugin active.
    let installed = peckboard::plugin::crates::all_ids();
    peckboard::plugin::crates::write_installed(&db, &installed).await;
    peckboard::plugin::crates::register_all(
        &builtin,
        registry.clone(),
        &db,
        plugins.clone(),
        &installed,
    )
    .await;
    plugins.set_provider_registry(registry);
    plugins.sync_plugin_providers().await;
    plugins
}
