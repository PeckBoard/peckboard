//! Built-in plugin that wraps the Grok CLI agent provider.
//!
//! Registers a `grok` provider whose model strings are `grok:<model>`. The
//! actual provider lives in [`crate::provider::grok`]; this module is a thin
//! permission-aware wrapper so the agent provider is discoverable through the
//! plugin catalog. Unlike the Claude plugin there is no idle-process reaper —
//! grok is invoked once per turn and the child exits when the turn ends.
//!
//! Settings exposed to the UI:
//!
//! * `cli_path` (string) — path to the `grok` binary. Defaults to `grok`
//!   (resolved on `PATH`, then the usual install locations).
//! * `discover_models` (boolean, default `true`) — ask the CLI (`grok models`)
//!   for its auth-scoped catalog and show those in the picker. Falls back to
//!   the built-in seed plus `additional_models` when discovery is off or fails.
//! * `additional_models` (string list) — extra model ids to surface in the
//!   picker, merged on top of the discovered/seed list as `grok:<id>`.

use async_trait::async_trait;
use std::sync::Arc;

use crate::plugin::builtin::{BuiltinPlugin, Permission, PluginInitContext, PluginMetadata};
use crate::plugin::settings::{FieldKind, SettingField, SettingsSchema};
use crate::provider::grok::{GrokProvider, default_models};
use crate::provider::registry::{
    AnswerTransport, InterruptKind, ProviderCapabilities, ProviderInfo, standard_effort_levels,
};

pub struct GrokPlugin;

impl GrokPlugin {
    fn schema() -> SettingsSchema {
        SettingsSchema::new(vec![
            SettingField {
                key: "cli_path".into(),
                title: "CLI Path".into(),
                description: Some(
                    "Path to the grok binary. Leave as grok to resolve it on your PATH \
                     (the provider also checks ~/.local/bin, ~/.npm-global/bin, \
                     ~/.bun/bin and /usr/local/bin, since the server's PATH often \
                     predates the install), or give an absolute path."
                        .into(),
                ),
                required: false,
                kind: FieldKind::String {
                    secret: false,
                    default: Some("grok".into()),
                    placeholder: Some("grok".into()),
                },
            },
            SettingField {
                key: "discover_models".into(),
                title: "Auto-Discover Models".into(),
                description: Some(
                    "Ask the grok CLI (grok models) which models are available for \
                     the current login and list them in the model picker. Turn this \
                     off to show only the built-in suggestions plus any models you \
                     add below."
                        .into(),
                ),
                required: false,
                kind: FieldKind::Boolean { default: true },
            },
            SettingField {
                key: "additional_models".into(),
                title: "Additional Models".into(),
                description: Some(
                    "Extra model ids to add to the picker on top of the auto-discovered \
                     (or built-in) list. Each appears as grok:<id>."
                        .into(),
                ),
                required: false,
                kind: FieldKind::StringList {
                    item_placeholder: Some("grok-4.6".into()),
                },
            },
        ])
    }
}

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

    fn settings_schema(&self) -> SettingsSchema {
        Self::schema()
    }

    async fn init(&self, ctx: &PluginInitContext) -> anyhow::Result<()> {
        ctx.require(Permission::RegisterProvider)?;
        ctx.require(Permission::SpawnProcess)?;

        // The provider re-reads settings on every dispatch, so a UI edit
        // takes effect on the next turn without restarting Peckboard.
        let store = ctx.settings_store(Self::schema());
        let provider = Arc::new(
            GrokProvider::new()
                .with_db(ctx.db.clone())
                .with_settings(store),
        );

        ctx.provider_registry
            .register(
                provider,
                ProviderInfo {
                    id: "grok".into(),
                    display_name: "Grok (CLI)".into(),
                    models: default_models(),
                    effort_levels: standard_effort_levels(),
                    // Per-turn CLI: attachments are dropped, no Usage
                    // events are parsed, and answers arrive as a fresh
                    // turn. `--session-id` resumes the conversation across
                    // turns. Interrupt sends SIGINT and drains stdout for
                    // a couple seconds (`turn::graceful_cancel`) before
                    // SIGKILL, so a frame the CLI flushes in response
                    // still reaches the transcript — but grok has no
                    // stdin/control channel to settle a turn in-band, so
                    // this stays `HardKill`, not `Soft`.
                    capabilities: ProviderCapabilities {
                        supports_thinking: true,
                        supports_images_in: false,
                        supports_usage: false,
                        supports_resume: true,
                        interrupt_kind: InterruptKind::HardKill,
                        supports_mid_stream_injection: false,
                        answer_transport: AnswerTransport::NewTurn,
                    },
                },
            )
            .await;

        Ok(())
    }
}
