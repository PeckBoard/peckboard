//! Built-in plugin that wraps the Codex CLI agent provider.
//!
//! Registers a `codex` provider whose model strings are `codex:<model>`.
//! The actual provider lives in [`crate::provider::codex`]; this module is a
//! thin permission-aware wrapper so the agent provider is discoverable
//! through the plugin catalog. Like Grok, Codex is invoked once per turn
//! (`codex exec`) and the child exits when the turn ends.
//!
//! Settings exposed to the UI:
//!
//! * `cli_path` (string) — path to the `codex` binary. Defaults to `codex`
//!   (resolved on `PATH`, then the usual install locations).
//! * `discover_models` (boolean, default `true`) — ask the CLI
//!   (`codex debug models --bundled`) for its catalog and show those in the
//!   picker. Falls back to the built-in seed plus `additional_models` when
//!   discovery is off or fails.
//! * `api_key` (secret string) — injected as `CODEX_API_KEY` at spawn time.
//!   The zero-config path is signing in on the host with `codex login`
//!   (writes `~/.codex/auth.json`).
//! * `additional_models` (string list) — extra model ids to surface in the
//!   picker, merged on top of the discovered/seed list as `codex:<id>`.

use async_trait::async_trait;
use std::sync::Arc;

use crate::plugin::builtin::{BuiltinPlugin, Permission, PluginInitContext, PluginMetadata};
use crate::plugin::settings::{FieldKind, SettingField, SettingsSchema};
use crate::provider::codex::{CodexProvider, default_models};
use crate::provider::registry::{
    AnswerTransport, InterruptKind, ProviderCapabilities, ProviderInfo, standard_effort_levels,
};

pub struct CodexPlugin;

impl CodexPlugin {
    fn schema() -> SettingsSchema {
        SettingsSchema::new(vec![
            SettingField {
                key: "cli_path".into(),
                title: "CLI Path".into(),
                description: Some(
                    "Path to the codex binary. Leave as codex to resolve it on your PATH \
                     (the provider also checks ~/.local/bin, ~/.npm-global/bin, \
                     ~/.bun/bin and /usr/local/bin, since the server's PATH often \
                     predates the install), or give an absolute path. Install with: \
                     curl -fsSL https://chatgpt.com/codex/install.sh | sh"
                        .into(),
                ),
                required: false,
                kind: FieldKind::String {
                    secret: false,
                    default: Some("codex".into()),
                    placeholder: Some("codex".into()),
                },
            },
            SettingField {
                key: "discover_models".into(),
                title: "Auto-Discover Models".into(),
                description: Some(
                    "Ask the Codex CLI (codex debug models --bundled) which models are \
                     available and list them in the model picker. Turn this off to show \
                     only the built-in suggestions plus any models you add below."
                        .into(),
                ),
                required: false,
                kind: FieldKind::Boolean { default: true },
            },
            SettingField {
                key: "api_key".into(),
                title: "API Key".into(),
                description: Some(
                    "Optional OpenAI API key, injected as CODEX_API_KEY. The zero-config \
                     path is signing in on the host with `codex login` (writes \
                     ~/.codex/auth.json)."
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
                key: "additional_models".into(),
                title: "Additional Models".into(),
                description: Some(
                    "Extra model ids to add to the picker on top of the auto-discovered \
                     (or built-in) list. Each appears as codex:<id>."
                        .into(),
                ),
                required: false,
                kind: FieldKind::StringList {
                    item_placeholder: Some("gpt-5.6-terra".into()),
                },
            },
        ])
    }
}

#[async_trait]
impl BuiltinPlugin for CodexPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            id: "codex".into(),
            display_name: "Codex (CLI)".into(),
            description: "Drives sessions via the OpenAI Codex CLI (`codex exec --json`). \
                          Sign in on the host with `codex login`, or set an API key here."
                .into(),
            version: env!("PECKBOARD_VERSION").into(),
            author: "Peckboard".into(),
            built_in: true,
        }
    }

    fn requested_permissions(&self) -> Vec<Permission> {
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

        let store = ctx.settings_store(Self::schema());
        let provider = Arc::new(CodexProvider::new().with_settings(store));

        ctx.provider_registry
            .register(
                provider,
                ProviderInfo {
                    id: "codex".into(),
                    display_name: "Codex (CLI)".into(),
                    models: default_models(),
                    effort_levels: standard_effort_levels(),
                    // Per-turn CLI: image attachments go out as `--image`,
                    // Usage events come from `turn.completed`, answers arrive
                    // as a fresh turn, and `codex exec resume <thread_id>`
                    // continues the conversation. Interrupt sends SIGINT and
                    // drains stdout (`turn::graceful_cancel`) before SIGKILL
                    // — Codex has no stdin/control channel, so this stays
                    // `HardKill`, not `Soft`.
                    capabilities: ProviderCapabilities {
                        supports_thinking: true,
                        supports_images_in: true,
                        supports_usage: true,
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
