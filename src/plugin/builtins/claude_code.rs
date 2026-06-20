//! Built-in plugin that wraps the Claude CLI agent provider.
//!
//! Registers a `claude` provider whose model strings are `claude:<model>`
//! and starts its idle-process reaper. The actual provider lives in
//! [`crate::provider::claude`]; this module is a thin permission-aware
//! wrapper so the agent provider is discoverable through the plugin
//! catalog.

use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

use crate::plugin::builtin::{BuiltinPlugin, Permission, PluginInitContext, PluginMetadata};
use crate::provider::claude::{ClaudeProvider, discover_models};
use crate::provider::registry::ProviderInfo;

pub struct ClaudeCodePlugin;

#[async_trait]
impl BuiltinPlugin for ClaudeCodePlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            id: "claude-code".into(),
            display_name: "Claude Code".into(),
            description: "Drives sessions via the Claude CLI in stream-json mode.".into(),
            version: env!("PECKBOARD_VERSION").into(),
            author: "Peckboard".into(),
            built_in: true,
        }
    }

    fn requested_permissions(&self) -> Vec<Permission> {
        // The CLI spawns a subprocess, reads/writes the working dir, and
        // talks to Anthropic. The narrower DB permissions aren't requested
        // because the provider only emits events through the shared
        // `emit_event` helper, which has its own privileged seam.
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

        let provider = Arc::new(ClaudeProvider::new());
        // 30 minute idle window matches the previous standalone
        // `register_claude_provider` behavior — gives the user a
        // meaningful "back from a meeting" window without keeping a CLI
        // child alive forever.
        provider.spawn_idle_reaper(30 * 60 * 1_000, Duration::from_secs(60));

        ctx.provider_registry
            .register(
                provider,
                ProviderInfo {
                    id: "claude".into(),
                    display_name: "Claude (CLI)".into(),
                    models: discover_models(),
                },
            )
            .await;

        Ok(())
    }
}
