//! Built-in plugin that wraps the Grok CLI agent provider.
//!
//! Registers a `grok` provider whose model strings are `grok:<model>`. The
//! actual provider lives in [`crate::provider::grok`]; this module is a thin
//! permission-aware wrapper so the agent provider is discoverable through the
//! plugin catalog. Unlike the Claude plugin there is no idle-process reaper —
//! grok is invoked once per turn and the child exits when the turn ends.

use async_trait::async_trait;
use std::sync::Arc;

use crate::plugin::builtin::{BuiltinPlugin, Permission, PluginInitContext, PluginMetadata};
use crate::provider::grok::{GrokProvider, default_models};
use crate::provider::registry::ProviderInfo;

pub struct GrokPlugin;

#[async_trait]
impl BuiltinPlugin for GrokPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            id: "grok".into(),
            display_name: "Grok".into(),
            description: "Drives sessions via the Grok CLI in streaming-json mode.".into(),
            version: env!("PECKBOARD_VERSION").into(),
            author: "Peckboard".into(),
            built_in: true,
        }
    }

    fn requested_permissions(&self) -> Vec<Permission> {
        // The CLI spawns a subprocess, reads/writes the working dir, and talks
        // to xAI. Mirrors the Claude plugin's permission set.
        vec![
            Permission::RegisterProvider,
            Permission::SpawnProcess,
            Permission::FilesystemRead,
            Permission::FilesystemWrite,
            Permission::NetworkAccess,
        ]
    }

    async fn init(&self, ctx: &PluginInitContext) -> anyhow::Result<()> {
        ctx.require(Permission::RegisterProvider)?;
        ctx.require(Permission::SpawnProcess)?;

        let provider = Arc::new(GrokProvider::new().with_db(ctx.db.clone()));

        ctx.provider_registry
            .register(
                provider,
                ProviderInfo {
                    id: "grok".into(),
                    display_name: "Grok (CLI)".into(),
                    models: default_models(),
                },
            )
            .await;

        Ok(())
    }
}
