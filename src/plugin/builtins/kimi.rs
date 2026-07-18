//! Built-in plugin that registers the [`KimiProvider`] (Moonshot AI's
//! `kimi` Kimi Code CLI) alongside its user-configurable settings.
//!
//! Settings exposed to the UI:
//!
//! * `cli_path` (string) — path to the `kimi` binary. Defaults to `kimi`
//!   (resolved on `PATH`).
//! * `default_model` (string) — config.toml model alias used when a session
//!   has no explicit `kimi:<alias>` selection. Optional; falls back to the
//!   CLI's own `default_model`.
//! * `discover_models` (boolean, default `true`) — ask the CLI
//!   (`kimi provider list --json`) for its configured model aliases and show
//!   those in the picker, on top of the config-default entry.
//! * `api_key` (secret string) — injected as `KIMI_API_KEY` at spawn time
//!   for config files using the documented env fallback.
//! * `base_url` (string) — injected as `KIMI_BASE_URL` at spawn time.
//! * `additional_models` (string list) — extra aliases to surface in the
//!   picker, merged on top of the discovered list as `kimi:<alias>`.

use async_trait::async_trait;
use std::sync::Arc;

use crate::plugin::builtin::{BuiltinPlugin, Permission, PluginInitContext, PluginMetadata};
use crate::plugin::settings::{FieldKind, SettingField, SettingsSchema};
use crate::provider::kimi::{KimiProvider, default_models};
use crate::provider::registry::ProviderInfo;

pub struct KimiPlugin;

impl KimiPlugin {
    /// Also used by the `/api/kimi-accounts` login route to resolve the
    /// `cli_path` setting for spawning `kimi login`.
    pub(crate) fn schema() -> SettingsSchema {
        SettingsSchema::new(vec![
            SettingField {
                key: "cli_path".into(),
                title: "CLI Path".into(),
                description: Some(
                    "Path to the kimi binary. Leave as kimi to resolve it on your PATH, \
                     or give an absolute path (the installer puts it at \
                     ~/.kimi-code/bin/kimi). Install with: curl -fsSL \
                     https://code.kimi.com/kimi-code/install.sh | bash"
                        .into(),
                ),
                required: false,
                kind: FieldKind::String {
                    secret: false,
                    default: Some("kimi".into()),
                    placeholder: Some("kimi".into()),
                },
            },
            SettingField {
                key: "default_model".into(),
                title: "Default Model".into(),
                description: Some(
                    "config.toml model alias used when a session doesn't specify \
                     kimi:<alias>. Leave blank to use the CLI's own default_model."
                        .into(),
                ),
                required: false,
                kind: FieldKind::String {
                    secret: false,
                    default: None,
                    placeholder: Some("kimi-for-coding".into()),
                },
            },
            SettingField {
                key: "discover_models".into(),
                title: "Auto-Discover Models".into(),
                description: Some(
                    "Ask the kimi CLI (kimi provider list --json) which model aliases \
                     are configured and list them in the model picker. Turn this off \
                     to show only the config-default entry plus any aliases you add \
                     below."
                        .into(),
                ),
                required: false,
                kind: FieldKind::Boolean { default: true },
            },
            SettingField {
                key: "api_key".into(),
                title: "API Key".into(),
                description: Some(
                    "Optional Moonshot AI API key, injected as KIMI_API_KEY for \
                     config files that use the documented env fallback. The \
                     zero-config path is signing in on the host with `kimi login` \
                     instead."
                        .into(),
                ),
                required: false,
                kind: FieldKind::String {
                    secret: true,
                    default: None,
                    placeholder: Some("sk-...".into()),
                },
            },
            SettingField {
                key: "base_url".into(),
                title: "Base URL".into(),
                description: Some(
                    "Optional API endpoint override, injected as KIMI_BASE_URL \
                     (e.g. https://api.moonshot.cn/v1 for the CN platform)."
                        .into(),
                ),
                required: false,
                kind: FieldKind::String {
                    secret: false,
                    default: None,
                    placeholder: Some("https://api.moonshot.ai/v1".into()),
                },
            },
            SettingField {
                key: "additional_models".into(),
                title: "Additional Models".into(),
                description: Some(
                    "Extra config.toml model aliases to add to the picker on top of \
                     the discovered list. Each appears as kimi:<alias>."
                        .into(),
                ),
                required: false,
                kind: FieldKind::StringList {
                    item_placeholder: Some("kimi-for-coding".into()),
                },
            },
        ])
    }
}

#[async_trait]
impl BuiltinPlugin for KimiPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            id: "kimi".into(),
            display_name: "Kimi Code".into(),
            description: "Drives sessions through Moonshot AI's Kimi Code CLI in prompt mode."
                .into(),
            version: env!("PECKBOARD_VERSION").into(),
            author: "Peckboard".into(),
            built_in: true,
        }
    }

    fn requested_permissions(&self) -> Vec<Permission> {
        // The CLI spawns a subprocess, reads/writes the working dir, and
        // talks to Moonshot AI over the network.
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
        let provider = Arc::new(KimiProvider::new(store).with_db(ctx.db.clone()));

        ctx.provider_registry
            .register(
                provider,
                ProviderInfo {
                    id: "kimi".into(),
                    display_name: "Kimi Code".into(),
                    models: default_models(),
                    // The kimi CLI exposes no reasoning-effort flag; thinking
                    // is a property of the configured model alias.
                    effort_levels: vec![],
                },
            )
            .await;

        Ok(())
    }
}
