//! Seed catalog copied from `src/provider/claude/mod.rs::static_models`.

pub const PROVIDER_ID: &str = "claude";
pub const DISPLAY_NAME: &str = "Claude (CLI)";

pub fn seed_models() -> serde_json::Value {
    serde_json::json!([
        {
            "id": "claude-fable-5",
            "display_name": "Claude Fable 5",
            "capabilities": ["code", "reasoning", "vision"],
            "tier": 4
        },
        {
            "id": "claude-opus-5",
            "display_name": "Claude Opus 5",
            "capabilities": ["code", "reasoning", "vision"],
            "tier": 3
        },
        {
            "id": "claude-opus-4-8",
            "display_name": "Claude Opus 4.8",
            "capabilities": ["code", "reasoning", "vision"],
            "tier": 3
        },
        {
            "id": "claude-opus-4-7",
            "display_name": "Claude Opus 4.7",
            "capabilities": ["code", "reasoning", "vision"],
            "tier": 3
        },
        {
            "id": "claude-opus-4-6",
            "display_name": "Claude Opus 4.6",
            "capabilities": ["code", "reasoning", "vision"],
            "tier": 3
        },
        {
            "id": "claude-sonnet-5",
            "display_name": "Claude Sonnet 5",
            "capabilities": ["code", "vision"],
            "tier": 2
        },
        {
            "id": "claude-sonnet-4-6",
            "display_name": "Claude Sonnet 4.6",
            "capabilities": ["code", "vision"],
            "tier": 2
        },
        {
            "id": "claude-haiku-4-5",
            "display_name": "Claude Haiku 4.5",
            "capabilities": ["code"],
            "tier": 1
        }
    ])
}
