//! Built-in plugin system.
//!
//! This is the in-process, statically-linked counterpart to the WASM extism
//! plugins in [`super::manager`]. A built-in plugin is a Rust module that
//! ships with Peckboard, declares the [`Permission`]s it needs, and at
//! startup registers its capabilities (currently: agent providers) through
//! a [`PluginInitContext`] that gates each action on a permission grant.
//!
//! Built-in plugins are always enabled. The grant rule is intentionally
//! simple: a built-in plugin receives every permission it requests, and
//! every permission is recorded in the catalog so the Settings UI can
//! display it. Future plugins (third-party WASM, sideloaded Rust crates)
//! will reuse the same `Permission` vocabulary with a different grant
//! policy.
//!
//! The catalog itself ([`BuiltinPluginRegistry`]) is read-only at runtime:
//! everything is registered once during startup, then the catalog is just
//! a snapshot the `/api/plugins` route serves.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::db::Db;
use crate::plugin::settings::{PluginSettingsStore, SettingsSchema};
use crate::provider::registry::ProviderRegistry;

/// Capability a built-in plugin can request at registration time.
///
/// The set is deliberately small but covers the rough surfaces a plugin
/// can touch — process, network, filesystem, the hook dispatcher, the
/// database, and (the only one currently exercised) registering an
/// AgentProvider. Adding finer-grained permissions is fine, but every
/// new variant must keep `label` + `description` populated, since the UI
/// renders directly from those.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Register an `AgentProvider` so its `provider:model` strings become
    /// valid for sessions/cards.
    RegisterProvider,
    /// Spawn OS subprocesses (e.g. wrapping a CLI agent).
    SpawnProcess,
    /// Make outbound network requests on behalf of Peckboard.
    NetworkAccess,
    /// Read files outside the session's working directory.
    FilesystemRead,
    /// Write files outside the session's working directory.
    FilesystemWrite,
    /// Register a handler that intercepts card lifecycle hooks
    /// (`card.create.before`, `card.update.before`, …).
    HookCardLifecycle,
    /// Register a handler that intercepts MCP tool calls and token
    /// minting hooks.
    HookMcpTools,
    /// Emit normalized `todo` snapshots through the shared todo hook.
    HookTodoTracking,
    /// Read rows from Peckboard's SQLite database.
    DatabaseRead,
    /// Mutate rows in Peckboard's SQLite database.
    DatabaseWrite,
}

impl Permission {
    /// Short human-readable name shown in the Settings list.
    pub fn label(self) -> &'static str {
        match self {
            Permission::RegisterProvider => "Register provider",
            Permission::SpawnProcess => "Spawn process",
            Permission::NetworkAccess => "Network access",
            Permission::FilesystemRead => "Read files",
            Permission::FilesystemWrite => "Write files",
            Permission::HookCardLifecycle => "Card lifecycle hooks",
            Permission::HookMcpTools => "MCP tool hooks",
            Permission::HookTodoTracking => "Todo tracking",
            Permission::DatabaseRead => "Read database",
            Permission::DatabaseWrite => "Write database",
        }
    }

    /// Longer one-liner shown under the label in the Settings list.
    pub fn description(self) -> &'static str {
        match self {
            Permission::RegisterProvider => "Adds an AI provider that can drive agent sessions",
            Permission::SpawnProcess => "Spawns OS subprocesses (e.g. CLI agents)",
            Permission::NetworkAccess => "Makes outbound network requests",
            Permission::FilesystemRead => "Reads files on the host filesystem",
            Permission::FilesystemWrite => "Writes or modifies files on the host filesystem",
            Permission::HookCardLifecycle => "Intercepts card create/update lifecycle events",
            Permission::HookMcpTools => "Intercepts MCP tool calls and token minting",
            Permission::HookTodoTracking => "Reports todo/task snapshots for sessions",
            Permission::DatabaseRead => "Reads from the Peckboard database",
            Permission::DatabaseWrite => "Writes to the Peckboard database",
        }
    }
}

/// Display metadata for a plugin. The UI renders directly from this shape,
/// so changes here ripple into `PluginsSection.tsx` — keep them in sync.
#[derive(Debug, Clone, Serialize)]
pub struct PluginMetadata {
    pub id: String,
    pub display_name: String,
    pub description: String,
    pub version: String,
    pub author: String,
    /// True for plugins compiled into the Peckboard binary. The UI uses
    /// this to render "Built-in · always enabled" and hide future
    /// toggle/uninstall controls.
    pub built_in: bool,
}

/// Status reported in the catalog after the plugin's `init` runs.
///
/// `init` failures are not fatal — the plugin is still listed so the user
/// can see why it isn't doing anything. The `message` carries the error
/// string for the UI to surface.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "message")]
pub enum PluginStatus {
    Active,
    InitFailed(String),
}

/// Handles a built-in plugin uses during `init` to act on its granted
/// permissions. Carries the provider registry (to register a new
/// AgentProvider), the granted permission set, and a `Db` handle so
/// plugins that expose settings can construct a [`PluginSettingsStore`]
/// for their provider.
pub struct PluginInitContext {
    pub provider_registry: Arc<ProviderRegistry>,
    pub granted_permissions: HashSet<Permission>,
    pub db: Db,
    /// Plugin id this context was built for. Used by [`settings_store`]
    /// so the plugin author doesn't have to repeat the id when wiring
    /// up its settings handle.
    pub plugin_id: String,
}

impl PluginInitContext {
    /// Bail with a descriptive error if the plugin hasn't been granted
    /// `permission`. Built-in plugins receive every permission they
    /// request, so this is effectively a typo-catcher for the plugin
    /// author: forget to declare `RegisterProvider` and `init` fails
    /// loudly instead of silently no-op'ing.
    pub fn require(&self, permission: Permission) -> anyhow::Result<()> {
        if !self.granted_permissions.contains(&permission) {
            return Err(anyhow::anyhow!(
                "plugin lacks required permission: {:?}",
                permission
            ));
        }
        Ok(())
    }

    /// Build a settings store handle the plugin's provider can hold to
    /// fetch effective values at request time.
    pub fn settings_store(&self, schema: SettingsSchema) -> PluginSettingsStore {
        PluginSettingsStore::new(self.plugin_id.clone(), schema, self.db.clone())
    }
}

/// One built-in plugin. Implementors live in `src/plugin/builtins/`.
#[async_trait]
pub trait BuiltinPlugin: Send + Sync + 'static {
    fn metadata(&self) -> PluginMetadata;
    fn requested_permissions(&self) -> Vec<Permission>;
    /// Settings schema this plugin exposes. Default: no settings.
    /// Returned in the `/api/plugins` payload and used by the Settings
    /// UI to render typed input controls.
    fn settings_schema(&self) -> SettingsSchema {
        SettingsSchema::default()
    }
    /// Register the plugin's capabilities. Called once at startup. Any
    /// error is captured into [`PluginStatus::InitFailed`] but does not
    /// abort startup.
    async fn init(&self, ctx: &PluginInitContext) -> anyhow::Result<()>;
}

/// Catalog entry exposed by `/api/plugins`. Mirrors what the UI consumes;
/// the underlying `Arc<dyn BuiltinPlugin>` is *not* serialized — only the
/// public metadata.
#[derive(Debug, Clone, Serialize)]
pub struct PluginCatalogEntry {
    #[serde(flatten)]
    pub metadata: PluginMetadata,
    pub permissions: Vec<PermissionInfo>,
    pub status: PluginStatus,
    pub enabled: bool,
    /// Typed settings schema. Empty when the plugin has no
    /// configurable surface — the UI hides the section in that case.
    pub settings_schema: SettingsSchema,
}

#[derive(Debug, Clone, Serialize)]
pub struct PermissionInfo {
    pub id: Permission,
    pub label: &'static str,
    pub description: &'static str,
}

impl From<Permission> for PermissionInfo {
    fn from(value: Permission) -> Self {
        PermissionInfo {
            id: value,
            label: value.label(),
            description: value.description(),
        }
    }
}

struct RegisteredPlugin {
    metadata: PluginMetadata,
    permissions: Vec<Permission>,
    status: PluginStatus,
    settings_schema: SettingsSchema,
}

/// Catalog of built-in plugins. Populated at startup by `register_and_init`
/// and then read-only for the rest of the process.
pub struct BuiltinPluginRegistry {
    plugins: Mutex<Vec<RegisteredPlugin>>,
}

impl BuiltinPluginRegistry {
    pub fn new() -> Self {
        BuiltinPluginRegistry {
            plugins: Mutex::new(Vec::new()),
        }
    }

    /// Register a plugin and immediately invoke its `init`. Built-in
    /// plugins receive every permission they request — there's no
    /// approval flow because the binary itself is the trust boundary —
    /// but the granted set is recorded so the UI can render it.
    pub async fn register_and_init(
        &self,
        plugin: Arc<dyn BuiltinPlugin>,
        provider_registry: Arc<ProviderRegistry>,
        db: Db,
    ) {
        let metadata = plugin.metadata();
        let requested = plugin.requested_permissions();
        let granted: HashSet<Permission> = requested.iter().copied().collect();
        let settings_schema = plugin.settings_schema();

        tracing::info!(
            plugin_id = %metadata.id,
            permissions = ?requested,
            "Initializing built-in plugin"
        );

        let ctx = PluginInitContext {
            provider_registry,
            granted_permissions: granted,
            db,
            plugin_id: metadata.id.clone(),
        };

        let status = match plugin.init(&ctx).await {
            Ok(()) => PluginStatus::Active,
            Err(e) => {
                tracing::error!(
                    plugin_id = %metadata.id,
                    "Plugin init failed: {e}"
                );
                PluginStatus::InitFailed(e.to_string())
            }
        };

        self.plugins.lock().await.push(RegisteredPlugin {
            metadata,
            permissions: requested,
            status,
            settings_schema,
        });
    }

    /// Snapshot of the catalog for `/api/plugins` and the UI.
    pub async fn list(&self) -> Vec<PluginCatalogEntry> {
        let plugins = self.plugins.lock().await;
        plugins
            .iter()
            .map(|p| PluginCatalogEntry {
                metadata: p.metadata.clone(),
                permissions: p.permissions.iter().copied().map(Into::into).collect(),
                status: p.status.clone(),
                // Built-in plugins are always enabled today; the field is
                // present so the UI doesn't have to special-case it once
                // third-party plugins start landing.
                enabled: matches!(p.status, PluginStatus::Active),
                settings_schema: p.settings_schema.clone(),
            })
            .collect()
    }

    /// Look up the settings schema for `plugin_id`. Used by the
    /// `/api/plugins/:id/settings` route to validate PUT payloads.
    pub async fn settings_schema_for(&self, plugin_id: &str) -> Option<SettingsSchema> {
        let plugins = self.plugins.lock().await;
        plugins
            .iter()
            .find(|p| p.metadata.id == plugin_id)
            .map(|p| p.settings_schema.clone())
    }
}

impl Default for BuiltinPluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyPlugin {
        id: &'static str,
        perms: Vec<Permission>,
    }

    #[async_trait]
    impl BuiltinPlugin for DummyPlugin {
        fn metadata(&self) -> PluginMetadata {
            PluginMetadata {
                id: self.id.to_string(),
                display_name: self.id.to_string(),
                description: "test".into(),
                version: "0.0.0".into(),
                author: "tests".into(),
                built_in: true,
            }
        }
        fn requested_permissions(&self) -> Vec<Permission> {
            self.perms.clone()
        }
        async fn init(&self, ctx: &PluginInitContext) -> anyhow::Result<()> {
            for p in &self.perms {
                ctx.require(*p)?;
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn registry_grants_requested_permissions() {
        let registry = Arc::new(ProviderRegistry::new());
        let catalog = BuiltinPluginRegistry::new();
        let db = Db::in_memory().unwrap();

        catalog
            .register_and_init(
                Arc::new(DummyPlugin {
                    id: "dummy",
                    perms: vec![Permission::RegisterProvider, Permission::SpawnProcess],
                }),
                registry,
                db,
            )
            .await;

        let entries = catalog.list().await;
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0].status, PluginStatus::Active));
        assert_eq!(entries[0].permissions.len(), 2);
        assert!(entries[0].enabled);
    }

    #[tokio::test]
    async fn init_failure_is_recorded_not_fatal() {
        struct Failing;
        #[async_trait]
        impl BuiltinPlugin for Failing {
            fn metadata(&self) -> PluginMetadata {
                PluginMetadata {
                    id: "failing".into(),
                    display_name: "failing".into(),
                    description: "test".into(),
                    version: "0.0.0".into(),
                    author: "tests".into(),
                    built_in: true,
                }
            }
            fn requested_permissions(&self) -> Vec<Permission> {
                vec![]
            }
            async fn init(&self, _ctx: &PluginInitContext) -> anyhow::Result<()> {
                Err(anyhow::anyhow!("nope"))
            }
        }

        let registry = Arc::new(ProviderRegistry::new());
        let catalog = BuiltinPluginRegistry::new();
        let db = Db::in_memory().unwrap();
        catalog
            .register_and_init(Arc::new(Failing), registry, db)
            .await;

        let entries = catalog.list().await;
        assert_eq!(entries.len(), 1);
        match &entries[0].status {
            PluginStatus::InitFailed(msg) => assert!(msg.contains("nope")),
            other => panic!("expected InitFailed, got {other:?}"),
        }
        assert!(!entries[0].enabled);
    }
}
