use std::collections::HashMap;
use tokio::sync::Mutex;

use super::stream::ModelInfo;

/// Registered provider metadata.
#[derive(Debug, Clone)]
pub struct ProviderInfo {
    pub id: String,
    pub display_name: String,
    pub models: Vec<ModelInfo>,
}

/// Registry of all available AI providers and their models.
pub struct ProviderRegistry {
    providers: Mutex<HashMap<String, ProviderInfo>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        ProviderRegistry {
            providers: Mutex::new(HashMap::new()),
        }
    }

    /// Register a provider. Overwrites if the same ID already exists.
    pub async fn register(&self, info: ProviderInfo) {
        let mut providers = self.providers.lock().await;
        tracing::info!(
            "Registered provider '{}' ({}) with {} models",
            info.id,
            info.display_name,
            info.models.len()
        );
        providers.insert(info.id.clone(), info);
    }

    /// Get a provider by ID.
    pub async fn get_provider(&self, id: &str) -> Option<ProviderInfo> {
        let providers = self.providers.lock().await;
        providers.get(id).cloned()
    }

    /// List all registered providers.
    pub async fn list_providers(&self) -> Vec<ProviderInfo> {
        let providers = self.providers.lock().await;
        providers.values().cloned().collect()
    }

    /// List all models across all providers, with provider:model format IDs.
    pub async fn list_all_models(&self) -> Vec<(String, ModelInfo)> {
        let providers = self.providers.lock().await;
        let mut models = Vec::new();
        for provider in providers.values() {
            for model in &provider.models {
                let full_id = format!("{}:{}", provider.id, model.id);
                models.push((
                    full_id,
                    model.clone(),
                ));
            }
        }
        models
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_register_and_list() {
        let registry = ProviderRegistry::new();

        registry
            .register(ProviderInfo {
                id: "claude".into(),
                display_name: "Claude".into(),
                models: vec![
                    ModelInfo {
                        id: "opus".into(),
                        display_name: "Claude Opus".into(),
                        capabilities: vec!["code".into(), "reasoning".into()],
                    },
                    ModelInfo {
                        id: "sonnet".into(),
                        display_name: "Claude Sonnet".into(),
                        capabilities: vec!["code".into()],
                    },
                ],
            })
            .await;

        let providers = registry.list_providers().await;
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].models.len(), 2);

        let claude = registry.get_provider("claude").await;
        assert!(claude.is_some());

        let missing = registry.get_provider("openai").await;
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn test_list_all_models() {
        let registry = ProviderRegistry::new();

        registry
            .register(ProviderInfo {
                id: "claude".into(),
                display_name: "Claude".into(),
                models: vec![ModelInfo {
                    id: "opus".into(),
                    display_name: "Opus".into(),
                    capabilities: vec![],
                }],
            })
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
}
