//! Built-in plugin that registers the [`OllamaProvider`] alongside its
//! user-configurable settings.
//!
//! Settings exposed to the UI:
//!
//! * `base_url` (required URL) — where Ollama is listening. Defaults
//!   to `http://localhost:11434`, the upstream default.
//! * `default_model` (string) — model name to use when a session has no
//!   explicit `ollama:<model>` selection. Optional; falls back to
//!   `llama3.1` if neither setting nor session override is present.
//! * `request_timeout_secs` (integer, 1–3600) — per-turn timeout for
//!   the HTTP call. Defaults to 600s; Ollama on CPU can take a while
//!   to load a fresh model on first use.
//! * `discover_models` (boolean, default `true`) — when on, the provider
//!   asks the server which models it has installed (via the OpenAI-
//!   compatible `/v1/models` endpoint) and shows them in the picker
//!   automatically. Turn off to fall back to the built-in seed plus
//!   `additional_models` only.
//! * `additional_models` (string list) — extra model names to surface in
//!   the model picker, merged on top of the autodiscovered (or seed)
//!   list. Each is registered as `ollama:<name>` and may carry a tag
//!   (`llama3.1:8b`). Useful for a model you haven't pulled yet or when
//!   discovery is off. Reflected in `/api/models` live, without a
//!   restart, via the provider's `dynamic_models` override.
//! * `additional_headers` (key-value list, secret values) — extra HTTP
//!   headers attached to every request. Use this for a remote Ollama
//!   behind an auth proxy (`Authorization: Bearer …`); values are
//!   masked when the settings are read back through the API.

use async_trait::async_trait;
use std::sync::Arc;

use crate::plugin::builtin::{BuiltinPlugin, Permission, PluginInitContext, PluginMetadata};
use crate::plugin::settings::{FieldKind, SettingField, SettingsSchema};
use crate::provider::ollama::{OllamaProvider, default_models};
use crate::provider::registry::ProviderInfo;

pub struct OllamaPlugin;

impl OllamaPlugin {
    fn schema() -> SettingsSchema {
        SettingsSchema::new(vec![
            SettingField {
                key: "base_url".into(),
                title: "Base URL".into(),
                description: Some(
                    "Where your Ollama server is listening. Use http://localhost:11434 \
                     for a local install, or https://ollama.example.com if you've put it \
                     behind a proxy."
                        .into(),
                ),
                required: true,
                kind: FieldKind::Url {
                    default: Some("http://localhost:11434".into()),
                    placeholder: Some("http://localhost:11434".into()),
                },
            },
            SettingField {
                key: "default_model".into(),
                title: "Default Model".into(),
                description: Some(
                    "Model name used when a session doesn't specify ollama:<model>. \
                     Must already be pulled on the Ollama server (e.g. llama3.1, qwen2.5-coder)."
                        .into(),
                ),
                required: false,
                kind: FieldKind::String {
                    secret: false,
                    default: None,
                    placeholder: Some("llama3.1".into()),
                },
            },
            SettingField {
                key: "request_timeout_secs".into(),
                title: "Request Timeout (Seconds)".into(),
                description: Some(
                    "How long to wait for a single Ollama response. Increase if you're \
                     running large models on CPU and the first turn times out."
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
                    "Ask the Ollama server which models it has installed (via the \
                     OpenAI-compatible /v1/models endpoint) and list them in the model \
                     picker automatically. Turn this off to show only the built-in \
                     suggestions plus any models you add below."
                        .into(),
                ),
                required: false,
                kind: FieldKind::Boolean { default: true },
            },
            SettingField {
                key: "additional_models".into(),
                title: "Additional Models".into(),
                description: Some(
                    "Extra model names to add to the picker on top of the auto-discovered \
                     list (or the built-in suggestions when discovery is off). Use the exact \
                     name as pulled on your Ollama server, including any tag (e.g. \
                     llama3.1:8b, mistral-small, me/custom-model). Each appears as \
                     ollama:<name>."
                        .into(),
                ),
                required: false,
                kind: FieldKind::StringList {
                    item_placeholder: Some("llama3.1:8b".into()),
                },
            },
            SettingField {
                key: "additional_headers".into(),
                title: "Additional HTTP Headers".into(),
                description: Some(
                    "Extra headers attached to every request. Use this for an auth proxy \
                     (Authorization: Bearer …). Values are stored encrypted-at-rest only \
                     in the sense that they're not echoed back through the API once saved."
                        .into(),
                ),
                required: false,
                kind: FieldKind::KeyValueList {
                    secret_values: true,
                    key_placeholder: Some("Authorization".into()),
                    value_placeholder: Some("Bearer …".into()),
                },
            },
        ])
    }
}

#[async_trait]
impl BuiltinPlugin for OllamaPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            id: "ollama".into(),
            display_name: "Ollama".into(),
            description: "Drives sessions through an Ollama server's /api/chat endpoint.".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            author: "Peckboard".into(),
            built_in: true,
        }
    }

    fn requested_permissions(&self) -> Vec<Permission> {
        // No subprocess, no filesystem, no DB writes — just outbound
        // HTTP to wherever the user pointed `base_url`.
        vec![Permission::RegisterProvider, Permission::NetworkAccess]
    }

    fn settings_schema(&self) -> SettingsSchema {
        Self::schema()
    }

    async fn init(&self, ctx: &PluginInitContext) -> anyhow::Result<()> {
        ctx.require(Permission::RegisterProvider)?;
        ctx.require(Permission::NetworkAccess)?;

        // The provider holds a settings store handle and re-reads
        // settings on every dispatch, so a UI edit takes effect on the
        // next turn without restarting Peckboard.
        let store = ctx.settings_store(Self::schema());
        let provider = Arc::new(OllamaProvider::new(store));

        ctx.provider_registry
            .register(
                provider,
                ProviderInfo {
                    id: "ollama".into(),
                    display_name: "Ollama".into(),
                    models: default_models(),
                },
            )
            .await;

        Ok(())
    }
}
