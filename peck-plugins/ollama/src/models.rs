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

/// Local inference has no per-token billing. Registration pricing is a
/// static per-model map, so only the seed models can be declared free here;
/// discovered / additional / `@server` models stay unpriced (the old native
/// provider's `model_price` returned 0.0 for every id).
pub fn seed_pricing() -> serde_json::Value {
    serde_json::json!({
        "llama3.1": { "input_usd_per_mtok": 0.0, "output_usd_per_mtok": 0.0 },
        "llama3.2": { "input_usd_per_mtok": 0.0, "output_usd_per_mtok": 0.0 },
        "qwen2.5-coder": { "input_usd_per_mtok": 0.0, "output_usd_per_mtok": 0.0 }
    })
}
