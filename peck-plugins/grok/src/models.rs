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

/// Humanize a bare CLI model id for the picker (`grok-4.6` → `Grok 4.6`).
pub fn display_name(id: &str) -> String {
    match id.strip_prefix("grok-") {
        Some(rest) if !rest.is_empty() => format!("Grok {rest}"),
        _ => id.to_string(),
    }
}

/// One catalog entry for a discovered / additional model id.
pub fn model_json(id: &str, tier: i64) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "display_name": display_name(id),
        "capabilities": ["code", "reasoning"],
        "tier": tier,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_humanizes_grok_prefix() {
        assert_eq!(display_name("grok-4.6"), "Grok 4.6");
        assert_eq!(display_name("grok-4.5"), "Grok 4.5");
        assert_eq!(display_name("other"), "other");
    }
}
