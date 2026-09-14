//! Seed catalog copied from `src/provider/grok/mod.rs::default_models`.

pub const PROVIDER_ID: &str = "grok";
pub const DISPLAY_NAME: &str = "Grok (CLI)";

pub fn seed_models() -> serde_json::Value {
    serde_json::json!([
        {
            "id": "grok-4.6",
            "display_name": "Grok 4.6",
            "capabilities": ["code", "reasoning"],
            "tier": 0
        },
        {
            "id": "grok-4.5",
            "display_name": "Grok 4.5",
            "capabilities": ["code", "reasoning"],
            "tier": 1
        }
    ])
}
