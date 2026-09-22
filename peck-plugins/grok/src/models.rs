//! Compile-time seed catalog for the Grok CLI provider.
//!
//! Runtime discovery prefers the live `grok models` list. Successful
//! discoveries are cached as last-good; this seed is the offline fallback
//! and is also merged in so newly pinned flagship ids stay selectable when
//! the CLI catalog (or a stale last-good cache) is behind. Refresh with
//! `scripts/refresh-provider-model-seeds.sh --write` when authenticated.

pub const PROVIDER_ID: &str = "grok";
pub const DISPLAY_NAME: &str = "Grok (CLI)";

pub fn seed_models() -> serde_json::Value {
    serde_json::json!([
        {
            "id": "grok-4.7",
            "display_name": "Grok 4.7",
            "capabilities": ["code", "reasoning"],
            "tier": 0
        },
        {
            "id": "grok-4.6",
            "display_name": "Grok 4.6",
            "capabilities": ["code", "reasoning"],
            "tier": 1
        },
        {
            "id": "grok-4.5",
            "display_name": "Grok 4.5",
            "capabilities": ["code", "reasoning"],
            "tier": 2
        }
    ])
}

/// Humanize a bare CLI model id for the picker (`grok-4.7` → `Grok 4.7`).
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

/// Append any seed ids missing from `models` so pinned flagships stay in the
/// picker when live discovery or last-good is behind the seed.
pub fn merge_always_offered(mut models: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    let seed = seed_models();
    for extra in seed.as_array().cloned().unwrap_or_default() {
        let id = extra.get("id").and_then(|v| v.as_str());
        if !models
            .iter()
            .any(|m| m.get("id").and_then(|v| v.as_str()) == id)
        {
            models.push(extra);
        }
    }
    models
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_humanizes_grok_prefix() {
        assert_eq!(display_name("grok-4.7"), "Grok 4.7");
        assert_eq!(display_name("grok-4.6"), "Grok 4.6");
        assert_eq!(display_name("grok-4.5"), "Grok 4.5");
        assert_eq!(display_name("other"), "other");
    }

    #[test]
    fn merge_always_offered_tops_up_missing_seed_ids() {
        let partial = vec![model_json("grok-4.5", 0)];
        let merged = merge_always_offered(partial);
        let ids: Vec<&str> = merged
            .iter()
            .filter_map(|m| m.get("id").and_then(|v| v.as_str()))
            .collect();
        assert_eq!(ids[0], "grok-4.5");
        assert!(ids.contains(&"grok-4.7"));
        assert!(ids.contains(&"grok-4.6"));
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn seed_lists_flagship_first() {
        let seed = seed_models();
        let ids: Vec<&str> = seed
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m.get("id").and_then(|v| v.as_str()))
            .collect();
        assert_eq!(ids, vec!["grok-4.7", "grok-4.6", "grok-4.5"]);
    }
}
