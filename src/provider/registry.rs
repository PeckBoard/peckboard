use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::agent::AgentProvider;
use super::stream::ModelInfo;

/// Registered provider metadata.
#[derive(Debug, Clone)]
pub struct ProviderInfo {
    pub id: String,
    pub display_name: String,
    pub models: Vec<ModelInfo>,
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

    /// List all registered providers' metadata.
    pub async fn list_providers(&self) -> Vec<ProviderInfo> {
        let providers = self.providers.lock().await;
        providers.values().map(|r| r.info.clone()).collect()
    }

    /// List all models across all providers, with provider:model format IDs.
    pub async fn list_all_models(&self) -> Vec<(String, ModelInfo)> {
        let providers = self.providers.lock().await;
        let mut models = Vec::new();
        for registered in providers.values() {
            for model in &registered.info.models {
                let full_id = format!("{}:{}", registered.info.id, model.id);
                models.push((full_id, model.clone()));
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
                        },
                        ModelInfo {
                            id: "sonnet".into(),
                            display_name: "Claude Sonnet".into(),
                            capabilities: vec!["code".into()],
                        },
                    ],
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
                    }],
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
}
