//! Plugin manifest: identity, hooks, permissions, settings.
//! Settings copied from `src/plugin/builtins/codex.rs`.

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
                "description": "Path to the codex binary. Leave as codex to resolve it on your PATH (the provider also checks ~/.local/bin, ~/.npm-global/bin, ~/.bun/bin and /usr/local/bin, since the server's PATH often predates the install), or give an absolute path. Install with: curl -fsSL https://chatgpt.com/codex/install.sh | sh",
                "type": "string",
                "default": "codex",
                "placeholder": "codex"
            },
            {
                "key": "discover_models",
                "title": "Auto-Discover Models",
                "description": "Ask the Codex CLI (codex debug models --bundled) which models are available and list them in the model picker. Turn this off to show only the built-in suggestions plus any models you add below.",
                "type": "boolean",
                "default": true
            },
            {
                "key": "additional_models",
                "title": "Additional Models",
                "description": "Extra model ids to add to the picker on top of the auto-discovered (or built-in) list. Each appears as codex:<id>.",
                "type": "string_list",
                "item_placeholder": "gpt-5.6-terra"
            }
        ],
    })
    .to_string()
}
