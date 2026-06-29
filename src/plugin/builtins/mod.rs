//! Built-in plugins that ship with Peckboard.
//!
//! Each submodule defines one plugin (currently: `claude-code`, `mock`,
//! `ollama`). They are registered with the [`BuiltinPluginRegistry`] at
//! startup via [`register_all`], which is the single seam every binary
//! entry point (`main.rs`, integration tests) should use to wire the
//! catalog.

use std::sync::Arc;

use super::builtin::BuiltinPluginRegistry;
use crate::db::Db;
use crate::provider::registry::ProviderRegistry;

pub mod claude_code;
pub mod cursor;
pub mod grok;
pub mod mock;
pub mod ollama;

/// Register every built-in plugin in the catalog and run its `init`. The
/// catalog and the provider registry are mutated in place; callers hand in
/// the shared arcs and DB handle.
pub async fn register_all(
    catalog: &BuiltinPluginRegistry,
    provider_registry: Arc<ProviderRegistry>,
    db: Db,
) {
    catalog
        .register_and_init(
            Arc::new(claude_code::ClaudeCodePlugin),
            provider_registry.clone(),
            db.clone(),
        )
        .await;
    catalog
        .register_and_init(
            Arc::new(mock::MockPlugin),
            provider_registry.clone(),
            db.clone(),
        )
        .await;
    catalog
        .register_and_init(
            Arc::new(ollama::OllamaPlugin),
            provider_registry.clone(),
            db.clone(),
        )
        .await;
    catalog
        .register_and_init(
            Arc::new(cursor::CursorPlugin),
            provider_registry.clone(),
            db.clone(),
        )
        .await;
    catalog
        .register_and_init(Arc::new(grok::GrokPlugin), provider_registry, db)
        .await;
}
