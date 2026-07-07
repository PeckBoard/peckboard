use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::agent::AgentProvider;
use super::stream::ModelInfo;

/// One selectable reasoning-effort level a provider exposes.
///
/// `id` is the raw value handed to the provider (e.g. the CLI `--effort`
/// flag); `label` is the human-facing name shown in the effort picker.
/// Every provider supplies its own set — Claude and Grok expose the full
/// ladder, Cursor bakes effort into the model id, and Ollama/Mock have
/// none — so the UI can load a model's provider's levels the moment a
/// model is chosen.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffortLevel {
    pub id: String,
    pub label: String,
}

/// The standard reasoning-effort ladder shared by the Claude and Grok CLIs:
/// `low`, `medium`, `high`, `xhigh` (Extra high), `max`. Providers that
/// support the same set reuse this so the ladder is defined once.
pub fn standard_effort_levels() -> Vec<EffortLevel> {
    [
        ("low", "Low"),
        ("medium", "Medium"),
        ("high", "High"),
        ("xhigh", "Extra high"),
        ("max", "Max"),
    ]
    .into_iter()
    .map(|(id, label)| EffortLevel {
        id: id.into(),
        label: label.into(),
    })
    .collect()
}

/// Registered provider metadata.
#[derive(Debug, Clone)]
pub struct ProviderInfo {
    pub id: String,
    pub display_name: String,
    pub models: Vec<ModelInfo>,
    /// Effort levels this provider exposes for the effort picker. Empty when
    /// the provider has no reasoning-effort control (e.g. Ollama, or Cursor
    /// where effort is baked into the model id). The UI always prepends a
    /// "Default" option, so an empty list means "Default only".
    pub effort_levels: Vec<EffortLevel>,
}

struct RegisteredProvider {
    info: ProviderInfo,
    provider: Arc<dyn AgentProvider>,
}

/// Registry of all available AI providers and their models.
///
/// Holds both the metadata (for `/api/models`) and the trait object that
/// the dispatcher uses to actually drive a run.
pub struct ProviderRegistry {
    providers: Mutex<HashMap<String, RegisteredProvider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        ProviderRegistry {
            providers: Mutex::new(HashMap::new()),
        }
    }

    /// Register a provider implementation along with its metadata.
    /// Overwrites if the same ID already exists.
    pub async fn register(&self, provider: Arc<dyn AgentProvider>, info: ProviderInfo) {
        let mut providers = self.providers.lock().await;
        tracing::info!(
            "Registered provider '{}' ({}) with {} models",
            info.id,
            info.display_name,
            info.models.len()
        );
        providers.insert(info.id.clone(), RegisteredProvider { info, provider });
    }

    /// Get provider metadata by ID.
    pub async fn get_info(&self, id: &str) -> Option<ProviderInfo> {
        let providers = self.providers.lock().await;
        providers.get(id).map(|r| r.info.clone())
    }

    /// Get the provider implementation by ID.
    pub async fn get_provider(&self, id: &str) -> Option<Arc<dyn AgentProvider>> {
        let providers = self.providers.lock().await;
        providers.get(id).map(|r| r.provider.clone())
    }

    /// List all registered providers' metadata, with the **static** model
    /// list captured at init. Cheap (no provider calls) — this is the form
    /// used by the dispatch/fan-out paths that only need provider ids.
    /// Use [`list_providers_with_models`](Self::list_providers_with_models)
    /// for the UI catalog, where settings-derived models must be resolved.
    pub async fn list_providers(&self) -> Vec<ProviderInfo> {
        let providers = self.providers.lock().await;
        providers.values().map(|r| r.info.clone()).collect()
    }

    /// List all providers with their **effective** model list: a
    /// provider's [`dynamic_models`](super::agent::AgentProvider::dynamic_models)
    /// override (settings-derived, e.g. Ollama's user-registered extras)
    /// when it supplies one, else the static list from `ProviderInfo`.
    ///
    /// This is the catalog form the `/api/models` route and the MCP
    /// `list_models` tool consume so a settings change shows up without a
    /// restart. Calling `dynamic_models()` under the registry lock is safe
    /// — providers read their own settings store, never the registry.
    pub async fn list_providers_with_models(&self) -> Vec<ProviderInfo> {
        let providers = self.providers.lock().await;
        let mut out = Vec::with_capacity(providers.len());
        for registered in providers.values() {
            let models = match registered.provider.dynamic_models().await {
                Some(models) => models,
                None => registered.info.models.clone(),
            };
            out.push(ProviderInfo {
                models,
                ..registered.info.clone()
            });
        }
        out
    }

    /// List all models across all providers, with provider:model format
    /// IDs. Resolves each provider's effective (dynamic-or-static) model
    /// list, so settings-derived models are included.
    pub async fn list_all_models(&self) -> Vec<(String, ModelInfo)> {
        let providers = self.list_providers_with_models().await;
        let mut models = Vec::new();
        for info in &providers {
            for model in &info.models {
                let full_id = format!("{}:{}", info.id, model.id);
                models.push((full_id, model.clone()));
            }
        }
        models
    }
    /// The cheapest model `provider_id` offers, ranked by the provider's own
    /// published price (input + output USD per million tokens, via
    /// `AgentProvider::model_price`). `None` when the provider is unknown or
    /// prices none of its models — an unpriced model is unknown, never free.
    /// Ties keep the earlier catalog entry.
    pub async fn cheapest_model(&self, provider_id: &str) -> Option<String> {
        let (info, provider) = {
            let providers = self.providers.lock().await;
            let r = providers.get(provider_id)?;
            (r.info.clone(), r.provider.clone())
        };
        let models = match provider.dynamic_models().await {
            Some(models) => models,
            None => info.models,
        };
        let mut best: Option<(String, f64)> = None;
        for m in &models {
            if let Some((input, output)) = provider.model_price(&m.id) {
                let total = input + output;
                if best.as_ref().map_or(true, |(_, b)| total < *b) {
                    best = Some((m.id.clone(), total));
                }
            }
        }
        best.map(|(id, _)| id)
    }

    /// Parse a model ID. Returns (provider_id, model_id).
    /// If no prefix, uses the default provider.
    pub fn parse_model_id(model_id: &str, default_provider: &str) -> (String, String) {
        match model_id.split_once(':') {
            Some((provider, model)) => (provider.to_string(), model.to_string()),
            None => (default_provider.to_string(), model_id.to_string()),
        }
    }
}

/// Split a (possibly account-scoped) model id into its base model and the
/// Claude account id it targets.
///
/// Multi-account support folds the account into the model id with an `@`
/// suffix: `claude:claude-opus-4-8@acc_1a2b`. A model id with no `@` — every
/// session/card stored before multi-account, plus the implicit "Default"
/// account — yields `(model, None)`. `@` never appears in a Claude model id
/// or a Bedrock ARN, and account ids are generated without it, so the split
/// is unambiguous. Works on a full `claude:<model>@<acct>` or a bare
/// `<model>@<acct>` — the provider prefix carries no `@` either.
pub fn split_model_account(model_id: &str) -> (&str, Option<&str>) {
    match model_id.rsplit_once('@') {
        Some((model, acct)) if !acct.is_empty() => (model, Some(acct)),
        _ => (model_id, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::mock::MockProvider;

    #[tokio::test]
    async fn test_register_and_list() {
        let registry = ProviderRegistry::new();

        registry
            .register(
                Arc::new(MockProvider::new()),
                ProviderInfo {
                    id: "claude".into(),
                    display_name: "Claude".into(),
                    models: vec![
                        ModelInfo {
                            id: "opus".into(),
                            display_name: "Claude Opus".into(),
                            capabilities: vec!["code".into(), "reasoning".into()],
                            tier: 3,
                        },
                        ModelInfo {
                            id: "sonnet".into(),
                            display_name: "Claude Sonnet".into(),
                            capabilities: vec!["code".into()],
                            tier: 2,
                        },
                    ],
                    effort_levels: standard_effort_levels(),
                },
            )
            .await;

        let providers = registry.list_providers().await;
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].models.len(), 2);

        let claude = registry.get_info("claude").await;
        assert!(claude.is_some());

        let missing = registry.get_info("openai").await;
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn test_list_all_models() {
        let registry = ProviderRegistry::new();

        registry
            .register(
                Arc::new(MockProvider::new()),
                ProviderInfo {
                    id: "claude".into(),
                    display_name: "Claude".into(),
                    models: vec![ModelInfo {
                        id: "opus".into(),
                        display_name: "Opus".into(),
                        capabilities: vec![],
                        tier: 3,
                    }],
                    effort_levels: vec![],
                },
            )
            .await;

        let models = registry.list_all_models().await;
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].0, "claude:opus");
    }

    #[test]
    fn test_parse_model_id() {
        let (p, m) = ProviderRegistry::parse_model_id("claude:opus", "claude");
        assert_eq!(p, "claude");
        assert_eq!(m, "opus");

        let (p, m) = ProviderRegistry::parse_model_id("opus", "claude");
        assert_eq!(p, "claude");
        assert_eq!(m, "opus");

        let (p, m) = ProviderRegistry::parse_model_id("openai:gpt-4o", "claude");
        assert_eq!(p, "openai");
        assert_eq!(m, "gpt-4o");
    }

    #[test]
    fn test_split_model_account() {
        // Account-scoped: base model + account id.
        assert_eq!(
            split_model_account("claude-opus-4-8@acc_1a2b"),
            ("claude-opus-4-8", Some("acc_1a2b"))
        );
        // Works on the full provider-prefixed form too.
        assert_eq!(
            split_model_account("claude:claude-opus-4-8@acc_1a2b"),
            ("claude:claude-opus-4-8", Some("acc_1a2b"))
        );
        // No suffix → Default account (backward compatible).
        assert_eq!(
            split_model_account("claude:claude-opus-4-8"),
            ("claude:claude-opus-4-8", None)
        );
        // A trailing `@` with no id is treated as no account, not an empty one.
        assert_eq!(
            split_model_account("claude-opus-4-8@"),
            ("claude-opus-4-8@", None)
        );
        // A Bedrock ARN (colons, slashes, no `@`) is left whole.
        assert_eq!(
            split_model_account("arn:aws:bedrock:us-east-1::model/x"),
            ("arn:aws:bedrock:us-east-1::model/x", None)
        );
    }

    #[tokio::test]
    async fn cheapest_model_ranks_by_provider_price() {
        let registry = ProviderRegistry::new();
        registry
            .register(
                Arc::new(MockProvider::new()),
                ProviderInfo {
                    id: "mock".into(),
                    display_name: "Mock".into(),
                    models: crate::provider::mock::mock_model_infos(),
                    effort_levels: vec![],
                },
            )
            .await;

        // `echo` (0.1 + 0.5) undercuts `happy-path` (1.0 + 5.0); the
        // unpriced scenarios never win even though they'd sort "free".
        assert_eq!(
            registry.cheapest_model("mock").await.as_deref(),
            Some("echo")
        );
        // Unknown provider → no answer.
        assert_eq!(registry.cheapest_model("nope").await, None);
    }
}
