//! Plugin manifest: identity, hooks, permissions, settings.

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
                "description": "Path to the claude binary. Leave as claude to resolve it on your PATH (the provider also checks ~/.local/bin, ~/.npm-global/bin, ~/.bun/bin and /usr/local/bin, since the server's PATH often predates the install), or give an absolute path.",
                "type": "string",
                "default": "claude",
                "placeholder": "claude"
            },
            {
                "key": "discover_models",
                "title": "Auto-Discover Models",
                "description": "Probe the Claude CLI so the picker can list account-scoped copies of the built-in catalog. Turn this off to show only the built-in suggestions plus any models you add below.",
                "type": "boolean",
                "default": true
            },
            {
                "key": "additional_models",
                "title": "Additional Models",
                "description": "Extra model ids to add to the picker on top of the built-in list. Each appears as claude:<id>.",
                "type": "string_list",
                "item_placeholder": "claude-opus-4-7"
            }
        ],
    })
    .to_string()
}
