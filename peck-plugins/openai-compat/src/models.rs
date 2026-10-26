//! Provider identity and seed catalog. An OpenAI-compatible server serves
//! whatever the operator loaded, so real models come from `/v1/models`
//! discovery and the Additional Models setting. The only seed is
//! [`DEFAULT_MODEL_ID`] — core requires at least one registered model, and
//! `openai-compat:default` resolves to the Default Model setting at send
//! time.

pub const PROVIDER_ID: &str = "openai-compat";
pub const DISPLAY_NAME: &str = "OpenAI-Compatible";
/// Placeholder id meaning "the Default Model setting".
pub const DEFAULT_MODEL_ID: &str = "default";

pub fn seed_models() -> serde_json::Value {
    serde_json::json!([
        {
            "id": DEFAULT_MODEL_ID,
            "display_name": "Default Model (OpenAI-Compatible)",
            "capabilities": ["code"],
            "tier": 0
        }
    ])
}

/// Display name for a discovered / additional model id.
pub fn display_name(id: &str) -> String {
    format!("{id} (OpenAI-Compatible)")
}
