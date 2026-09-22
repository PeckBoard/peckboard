//! Seed catalog copied from `src/provider/codex/mod.rs::default_models`.

pub const PROVIDER_ID: &str = "codex";
pub const DISPLAY_NAME: &str = "Codex (CLI)";

pub fn seed_models() -> serde_json::Value {
    serde_json::json!([
        {
            "id": "gpt-6-astra",
            "display_name": "GPT-6 Astra",
            "capabilities": ["code", "reasoning"],
            "tier": 0
        },
        {
            "id": "gpt-5.6-sol",
            "display_name": "GPT-5.6 Sol",
            "capabilities": ["code", "reasoning"],
            "tier": 1
        },
        {
            "id": "gpt-5.6-terra",
            "display_name": "GPT-5.6 Terra",
            "capabilities": ["code", "reasoning"],
            "tier": 2
        },
        {
            "id": "gpt-5.6-luna",
            "display_name": "GPT-5.6 Luna",
            "capabilities": ["code", "reasoning"],
            "tier": 3
        },
        {
            "id": "gpt-daybreak-blue-latest",
            "display_name": "Daybreak Blue",
            "capabilities": ["code", "reasoning"],
            "tier": 4
        },
        {
            "id": "gpt-daybreak-red-latest",
            "display_name": "Daybreak Red",
            "capabilities": ["code", "reasoning"],
            "tier": 5
        },
        {
            "id": "gpt-5.5",
            "display_name": "GPT-5.5",
            "capabilities": ["code", "reasoning"],
            "tier": 6
        },
        {
            "id": "gpt-5.4",
            "display_name": "GPT-5.4",
            "capabilities": ["code", "reasoning"],
            "tier": 7
        },
        {
            "id": "gpt-5.4-mini",
            "display_name": "GPT-5.4 Mini",
            "capabilities": ["code", "reasoning"],
            "tier": 8
        },
        {
            "id": "gpt-5.2",
            "display_name": "GPT-5.2",
            "capabilities": ["code", "reasoning"],
            "tier": 9
        },
        {
            "id": "codex-auto-review",
            "display_name": "Codex Auto Review",
            "capabilities": ["code", "reasoning"],
            "tier": 10
        }
    ])
}

/// Pretty display name for a discovered/extra model id, matching the old
/// native provider's `model_display_name`.
pub fn model_display_name(id: &str) -> String {
    match id {
        "gpt-5.6-luna" => "GPT-5.6 Luna".into(),
        "gpt-5.6-terra" => "GPT-5.6 Terra".into(),
        "gpt-5.6-sol" => "GPT-5.6 Sol".into(),
        "gpt-6-astra" => "GPT-6 Astra".into(),
        "gpt-daybreak-blue-latest" => "Daybreak Blue".into(),
        "gpt-daybreak-red-latest" => "Daybreak Red".into(),
        "gpt-5.4-mini" => "GPT-5.4 Mini".into(),
        "codex-auto-review" => "Codex Auto Review".into(),
        other => other.to_string(),
    }
}

/// Catalog entry for a discovered or user-added model id. Codex models are
/// all code+reasoning capable.
pub fn model_json(id: &str, tier: i32) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "display_name": model_display_name(id),
        "capabilities": ["code", "reasoning"],
        "tier": tier,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovered_ids_get_pretty_names_and_reasoning_tags() {
        let m = model_json("gpt-5.6-terra", 1);
        assert_eq!(m["display_name"], "GPT-5.6 Terra");
        assert_eq!(m["capabilities"], serde_json::json!(["code", "reasoning"]));
        assert_eq!(m["tier"], 1);
        assert_eq!(model_json("gpt-7-new", 0)["display_name"], "gpt-7-new");
    }
}
