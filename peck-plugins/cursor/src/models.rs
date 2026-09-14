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
            "id": "grok-4.5-high",
            "display_name": "Grok 4.5 High (Cursor)",
            "capabilities": ["code"],
            "tier": 0
        }
    ])
}
