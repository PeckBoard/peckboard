//! Built-in plugin that registers the [`CursorProvider`] (the `cursor-agent`
//! CLI) alongside its user-configurable settings.
//!
//! Settings exposed to the UI:
//!
//! * `cli_path` (string) — path to the `cursor-agent` binary. Defaults to
//!   `cursor-agent` (resolved on `PATH`).
//! * `default_model` (string) — model used when a session has no explicit
//!   `cursor:<model>` selection. Optional; falls back to `auto`.
//! * `request_timeout_secs` (integer, 1–3600) — per-turn timeout. Defaults
//!   to 600s.
//! * `discover_models` (boolean, default `true`) — ask the CLI for its model
//!   list and show those in the picker. Falls back to the built-in seed plus
//!   `additional_models` when discovery is off or fails.
//! * `auto_approve` (boolean, default `true`) — pass `--force` so the agent
//!   auto-approves tool actions in headless mode (no interactive prompts).
//! * `additional_models` (string list) — extra model ids to surface in the
//!   picker, merged on top of the discovered/seed list as `cursor:<id>`.

use async_trait::async_trait;
use std::sync::Arc;

use crate::plugin::builtin::{BuiltinPlugin, Permission, PluginInitContext, PluginMetadata};
use crate::plugin::settings::{FieldKind, SettingField, SettingsSchema};
use crate::provider::cursor::{CursorProvider, default_models};
use crate::provider::registry::ProviderInfo;

pub struct CursorPlugin;

impl CursorPlugin {
    fn schema() -> SettingsSchema {
        SettingsSchema::new(vec![
            SettingField {
                key: "cli_path".into(),
                title: "CLI Path".into(),
                description: Some(
                    "Path to the cursor-agent binary. Leave as cursor-agent to resolve \
                     it on your PATH, or give an absolute path to a specific install."
                        .into(),
                ),
                required: false,
                kind: FieldKind::String {
                    secret: false,
                    default: Some("cursor-agent".into()),
                    placeholder: Some("cursor-agent".into()),
                },
            },
            SettingField {
                key: "default_model".into(),
                title: "Default Model".into(),
                description: Some(
                    "Model used when a session doesn't specify cursor:<model>. Leave \
                     blank to let Cursor choose (auto)."
                        .into(),
                ),
                required: false,
                kind: FieldKind::String {
                    secret: false,
                    default: None,
                    placeholder: Some("auto".into()),
                },
            },
            SettingField {
                key: "request_timeout_secs".into(),
                title: "Turn Timeout (Seconds)".into(),
                description: Some(
                    "How long to let a single cursor-agent turn run before killing it. \
                     Increase for long multi-step agent runs."
                        .into(),
                ),
                required: false,
                kind: FieldKind::Integer {
                    default: Some(600),
                    min: Some(1),
                    max: Some(3600),
                },
            },
            SettingField {
                key: "discover_models".into(),
                title: "Auto-Discover Models".into(),
                description: Some(
                    "Ask the cursor-agent CLI which models are available and list them \
                     in the model picker. Turn this off to show only the built-in \
                     suggestions plus any models you add below."
                        .into(),
                ),
                required: false,
                kind: FieldKind::Boolean { default: true },
            },
            SettingField {
                key: "auto_approve".into(),
                title: "Auto-Approve Tool Actions".into(),
                description: Some(
                    "Pass --force so the agent runs tool actions without interactive \
                     approval prompts. Required for headless operation; turn off only if \
                     your cursor-agent version handles approvals differently."
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
                     (or built-in) list. Each appears as cursor:<id>."
                        .into(),
                ),
                required: false,
                kind: FieldKind::StringList {
                    item_placeholder: Some("gpt-5-codex".into()),
                },
            },
        ])
    }
}

#[async_trait]
impl BuiltinPlugin for CursorPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            id: "cursor".into(),
            display_name: "Cursor".into(),
            description: "Drives sessions through the cursor-agent CLI in print mode.".into(),
            version: env!("PECKBOARD_VERSION").into(),
            author: "Peckboard".into(),
            built_in: true,
        }
    }

    fn requested_permissions(&self) -> Vec<Permission> {
        // The CLI spawns a subprocess, reads/writes the working dir, and
        // talks to Cursor's backend over the network.
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
        let provider = Arc::new(CursorProvider::new(store));

        ctx.provider_registry
            .register(
                provider,
                ProviderInfo {
                    id: "cursor".into(),
                    display_name: "Cursor".into(),
                    models: default_models(),
                    // Cursor bakes the effort into the model id itself
                    // (e.g. `gpt-5.3-codex-high`), so there's no separate
                    // effort control to expose.
                    effort_levels: vec![],
                },
            )
            .await;

        Ok(())
    }
}
