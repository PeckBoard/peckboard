//! Seed catalog copied from `src/provider/ollama/mod.rs::default_models`.

pub const PROVIDER_ID: &str = "ollama";
pub const DISPLAY_NAME: &str = "Ollama";

pub fn seed_models() -> serde_json::Value {
    serde_json::json!([
        {
            "id": "llama3.1",
            "display_name": "Llama 3.1 (Ollama)",
            "capabilities": ["code"],
            "tier": 0
        },
        {
            "id": "llama3.2",
            "display_name": "Llama 3.2 (Ollama)",
            "capabilities": ["code"],
            "tier": 0
        },
        {
            "id": "qwen2.5-coder",
            "display_name": "Qwen 2.5 Coder (Ollama)",
            "capabilities": ["code"],
            "tier": 0
        }
    ])
}
