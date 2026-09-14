//! Plugin manifest: identity, hooks, permissions, settings.
//! Settings copied from `src/plugin/builtins/cursor.rs`.

pub fn manifest_json() -> String {
    serde_json::json!({
        "description": env!("CARGO_PKG_DESCRIPTION"),
        "version": env!("CARGO_PKG_VERSION"),
        "repository": env!("CARGO_PKG_REPOSITORY"),
        "hooks": [
            "provider.register",
            "provider.send",
            "provider.models",
            "provider.interrupt",
        ],
        "permissions": [
            "register_provider",
            "http_request",
            "data_store",
        ],
        "settings": [
            {
                "key": "cli_path",
                "title": "CLI Path",
                "description": "Path to the cursor-agent binary. Leave as cursor-agent to resolve it on your PATH, or give an absolute path to a specific install.",
                "type": "string",
                "default": "cursor-agent",
                "placeholder": "cursor-agent"
            },
            {
                "key": "default_model",
                "title": "Default Model",
                "description": "Model used when a session doesn't specify cursor:<model>. Leave blank to let Cursor choose (auto).",
                "type": "string",
                "placeholder": "auto"
            },
            {
                "key": "discover_models",
                "title": "Auto-Discover Models",
                "description": "Ask the cursor-agent CLI which models are available and list them in the model picker. Turn this off to show only the built-in suggestions plus any models you add below.",
                "type": "boolean",
                "default": true
            },
            {
                "key": "auto_approve",
                "title": "Auto-Approve Tool Actions",
                "description": "Pass --force so the agent runs tool actions without interactive approval prompts. Required for headless operation; turn off only if your cursor-agent version handles approvals differently.",
                "type": "boolean",
                "default": true
            },
            {
                "key": "additional_models",
                "title": "Additional Models",
                "description": "Extra model ids to add to the picker on top of the auto-discovered (or built-in) list. Each appears as cursor:<id>.",
                "type": "string_list",
                "item_placeholder": "gpt-5-codex"
            }
        ],
    })
    .to_string()
}
