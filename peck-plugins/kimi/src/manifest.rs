//! Plugin manifest: identity, hooks, permissions, settings.
//! Settings owned by this plugin's manifest.

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
                "description": "Path to the kimi binary. Leave as kimi to resolve it on your PATH, or give an absolute path (the installer puts it at ~/.kimi-code/bin/kimi). Install with: curl -fsSL https://code.kimi.com/kimi-code/install.sh | bash",
                "type": "string",
                "default": "kimi",
                "placeholder": "kimi"
            },
            {
                "key": "default_model",
                "title": "Default Model",
                "description": "config.toml model alias used when a session doesn't specify kimi:<alias>. Leave blank to use the CLI's own default_model.",
                "type": "string",
                "placeholder": "kimi-for-coding"
            },
            {
                "key": "discover_models",
                "title": "Auto-Discover Models",
                "description": "Ask the kimi CLI (kimi provider list --json) which model aliases are configured and list them in the model picker. Turn this off to show only the config-default entry plus any aliases you add below.",
                "type": "boolean",
                "default": true
            },
            {
                "key": "api_key",
                "title": "API Key",
                "description": "Optional Moonshot AI API key, injected as KIMI_API_KEY for config files that use the documented env fallback. The zero-config path is signing in on the host with `kimi login` instead.",
                "type": "string",
                "secret": true,
                "placeholder": "sk-..."
            },
            {
                "key": "base_url",
                "title": "Base URL",
                "description": "Optional API endpoint override, injected as KIMI_BASE_URL (e.g. https://api.moonshot.cn/v1 for the CN platform).",
                "type": "string",
                "placeholder": "https://api.moonshot.ai/v1"
            },
            {
                "key": "additional_models",
                "title": "Additional Models",
                "description": "Extra config.toml model aliases to add to the picker on top of the discovered list. Each appears as kimi:<alias>.",
                "type": "string_list",
                "item_placeholder": "kimi-for-coding"
            }
        ],
    })
    .to_string()
}
