//! Seed catalog copied from `src/provider/cursor/mod.rs::default_models`.

pub const PROVIDER_ID: &str = "cursor";
pub const DISPLAY_NAME: &str = "Cursor";

pub fn seed_models() -> serde_json::Value {
    serde_json::json!([
        {
            "id": "auto",
            "display_name": "Auto (Cursor)",
            "capabilities": ["code"],
            "tier": 0
        },
        {
            "id": "composer-2.5",
            "display_name": "Composer 2.5 (Cursor)",
            "capabilities": ["code"],
            "tier": 0
        },
        {
            "id": "composer-2.5-fast",
            "display_name": "Composer 2.5 Fast (Cursor)",
            "capabilities": ["code"],
            "tier": 0
        },
        {
            "id": "claude-opus-4-8-thinking-high",
            "display_name": "Claude Opus 4.8 Thinking (Cursor)",
            "capabilities": ["code", "reasoning"],
            "tier": 0
        },
        {
            "id": "claude-4.5-sonnet",
            "display_name": "Claude Sonnet 4.5 (Cursor)",
            "capabilities": ["code"],
            "tier": 0
        },
        {
            "id": "claude-4.5-sonnet-thinking",
            "display_name": "Claude Sonnet 4.5 Thinking (Cursor)",
            "capabilities": ["code", "reasoning"],
            "tier": 0
        },
        {
            "id": "gpt-5.5-high",
            "display_name": "GPT-5.5 High (Cursor)",
            "capabilities": ["code"],
            "tier": 0
        },
        {
            "id": "gpt-5.3-codex",
            "display_name": "Codex 5.3 (Cursor)",
            "capabilities": ["code"],
            "tier": 0
        },
        {
            "id": "gemini-3.1-pro",
            "display_name": "Gemini 3.1 Pro (Cursor)",
            "capabilities": ["code"],
            "tier": 0
        },
        {
            "id": "cursor-grok-4.5-high",
            "display_name": "Grok 4.5 (Cursor)",
            "capabilities": ["code"],
            "tier": 0
        }
    ])
}

/// Cursor encodes thinking in the model id itself (e.g.
/// `claude-opus-4-8-thinking-high`), so the catalog builder is where that
/// naming convention becomes an explicit `reasoning` capability tag.
fn model_capabilities(id: &str) -> Vec<&'static str> {
    if id.to_ascii_lowercase().contains("thinking") {
        vec!["code", "reasoning"]
    } else {
        vec!["code"]
    }
}

/// Catalog entry for a discovered or user-added model id, tagged `(Cursor)`
/// like the seed entries so the picker reads the same either way.
pub fn model_json(id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "display_name": format!("{id} (Cursor)"),
        "capabilities": model_capabilities(id),
        "tier": 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovered_ids_get_cursor_suffix_and_thinking_reasoning_tag() {
        let m = model_json("claude-4.6-sonnet-thinking");
        assert_eq!(m["display_name"], "claude-4.6-sonnet-thinking (Cursor)");
        assert_eq!(m["capabilities"], serde_json::json!(["code", "reasoning"]));
        let plain = model_json("gpt-5.3-codex");
        assert_eq!(plain["display_name"], "gpt-5.3-codex (Cursor)");
        assert_eq!(plain["capabilities"], serde_json::json!(["code"]));
    }
}
