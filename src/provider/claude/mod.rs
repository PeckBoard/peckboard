pub mod manager;
pub mod process;

use crate::provider::registry::{ProviderInfo, ProviderRegistry};
use crate::provider::stream::{ModelInfo, SpawnConfig};

/// Register the built-in Claude CLI provider in the registry.
pub async fn register_claude_provider(registry: &ProviderRegistry) {
    let models = discover_models();

    registry
        .register(ProviderInfo {
            id: "claude".into(),
            display_name: "Claude (CLI)".into(),
            models,
        })
        .await;
}

/// Discover available Claude models.
fn discover_models() -> Vec<ModelInfo> {
    let mut models = vec![
        ModelInfo {
            id: "claude-opus-4-8".into(),
            display_name: "Claude Opus 4.8".into(),
            capabilities: vec!["code".into(), "reasoning".into(), "vision".into()],
        },
        ModelInfo {
            id: "claude-opus-4-7".into(),
            display_name: "Claude Opus 4.7".into(),
            capabilities: vec!["code".into(), "reasoning".into(), "vision".into()],
        },
        ModelInfo {
            id: "claude-opus-4-6".into(),
            display_name: "Claude Opus 4.6".into(),
            capabilities: vec!["code".into(), "reasoning".into(), "vision".into()],
        },
        ModelInfo {
            id: "claude-sonnet-4-6".into(),
            display_name: "Claude Sonnet 4.6".into(),
            capabilities: vec!["code".into(), "vision".into()],
        },
        ModelInfo {
            id: "claude-haiku-4-5".into(),
            display_name: "Claude Haiku 4.5".into(),
            capabilities: vec!["code".into()],
        },
    ];

    // Check for Bedrock ARNs in environment
    for env_var in &[
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    ] {
        if let Ok(arn) = std::env::var(env_var) {
            if !models.iter().any(|m| m.id == arn) {
                models.push(ModelInfo {
                    id: arn.clone(),
                    display_name: format!("Bedrock: {}", arn.split('/').last().unwrap_or(&arn)),
                    capabilities: vec!["code".into()],
                });
            }
        }
    }

    models
}

/// Build the CLI arguments for spawning a Claude process.
pub fn build_cli_args(
    message: &str,
    config: &SpawnConfig,
    conversation_id: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "claude".to_string(),
        "-p".to_string(),
        message.to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
    ];

    if config.model != "default" {
        args.push("--model".to_string());
        args.push(config.model.clone());
    }

    if let Some(effort) = &config.effort {
        args.push("--effort".to_string());
        args.push(effort.clone());
    }

    if let Some(conv_id) = conversation_id {
        args.push("--resume".to_string());
        args.push(conv_id.to_string());
    }

    if let Some(mcp_path) = &config.mcp_config_path {
        args.push("--mcp-config".to_string());
        args.push(mcp_path.clone());
    }

    if let Some(mode) = &config.permission_mode {
        if mode == "bypass" {
            args.push("--permission-prompt-tool".to_string());
            args.push("stdio".to_string());
        }
    }

    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discover_models() {
        let models = discover_models();
        assert!(models.len() >= 5);
        assert!(models.iter().any(|m| m.id == "claude-opus-4-8"));
        assert!(models.iter().any(|m| m.id == "claude-sonnet-4-6"));
        assert!(models.iter().any(|m| m.id == "claude-haiku-4-5"));
    }

    #[test]
    fn test_build_cli_args_basic() {
        let config = SpawnConfig {
            model: "claude-opus-4-8".into(),
            effort: None,
            working_dir: "/tmp".into(),
            mcp_config_path: None,
            env: Default::default(),
            permission_mode: None,
            timeout_ms: None,
            metadata: serde_json::Value::Null,
        };

        let args = build_cli_args("hello", &config, None);
        assert!(args.contains(&"claude".to_string()));
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"hello".to_string()));
        assert!(args.contains(&"--model".to_string()));
        assert!(args.contains(&"claude-opus-4-8".to_string()));
        assert!(!args.contains(&"--resume".to_string()));
    }

    #[test]
    fn test_build_cli_args_with_resume() {
        let config = SpawnConfig {
            model: "claude-sonnet-4-6".into(),
            effort: Some("high".into()),
            working_dir: "/tmp".into(),
            mcp_config_path: Some("/tmp/mcp.json".into()),
            env: Default::default(),
            permission_mode: Some("bypass".into()),
            timeout_ms: None,
            metadata: serde_json::Value::Null,
        };

        let args = build_cli_args("hi", &config, Some("conv-123"));
        assert!(args.contains(&"--resume".to_string()));
        assert!(args.contains(&"conv-123".to_string()));
        assert!(args.contains(&"--effort".to_string()));
        assert!(args.contains(&"high".to_string()));
        assert!(args.contains(&"--mcp-config".to_string()));
        assert!(args.contains(&"--permission-prompt-tool".to_string()));
    }

    #[test]
    fn test_build_cli_args_default_model() {
        let config = SpawnConfig {
            model: "default".into(),
            effort: None,
            working_dir: "/tmp".into(),
            mcp_config_path: None,
            env: Default::default(),
            permission_mode: None,
            timeout_ms: None,
            metadata: serde_json::Value::Null,
        };

        let args = build_cli_args("test", &config, None);
        assert!(!args.contains(&"--model".to_string()));
    }
}
