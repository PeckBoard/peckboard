//! Seed catalog copied from `src/provider/kimi/mod.rs::default_models`.

pub const PROVIDER_ID: &str = "kimi";
pub const DISPLAY_NAME: &str = "Kimi Code";

pub fn seed_models() -> serde_json::Value {
    serde_json::json!([
        {
            "id": "default",
            "display_name": "Default (Kimi config)",
            "capabilities": ["code"],
            "tier": 0
        }
    ])
}
