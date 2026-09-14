//! Seed catalog copied from `src/provider/codex/mod.rs::default_models`.

pub const PROVIDER_ID: &str = "codex";
pub const DISPLAY_NAME: &str = "Codex (CLI)";

pub fn seed_models() -> serde_json::Value {
    serde_json::json!([
        {
            "id": "gpt-5.6-luna",
            "display_name": "GPT-5.6 Luna",
            "capabilities": ["code", "reasoning"],
            "tier": 0
        },
        {
            "id": "gpt-5.6-terra",
            "display_name": "GPT-5.6 Terra",
            "capabilities": ["code", "reasoning"],
            "tier": 1
        },
        {
            "id": "gpt-5.6-sol",
            "display_name": "GPT-5.6 Sol",
            "capabilities": ["code", "reasoning"],
            "tier": 2
        },
        {
            "id": "gpt-6-astra",
            "display_name": "GPT-6 Astra",
            "capabilities": ["code", "reasoning"],
            "tier": 3
        },
        {
            "id": "gpt-5.6",
            "display_name": "GPT-5.6",
            "capabilities": ["code", "reasoning"],
            "tier": 1
        }
    ])
}
