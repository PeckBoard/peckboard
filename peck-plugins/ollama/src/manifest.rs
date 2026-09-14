//! Plugin manifest: identity, hooks, permissions, settings.
//! Settings copied from `src/plugin/builtins/ollama.rs`.

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
                "key": "base_url",
                "title": "Default Server URL",
                "description": "Where your default Ollama server is listening. Use http://localhost:11434 for a local install, or https://ollama.example.com if you've put it behind a proxy. Models on this server appear under their bare name.",
                "required": true,
                "type": "url",
                "default": "http://localhost:11434",
                "placeholder": "http://localhost:11434"
            },
            {
                "key": "servers",
                "title": "Additional Servers",
                "description": "More Ollama servers, as name → base URL pairs. Models on a named server show up as model@name in the picker and can be selected as ollama:<model>@<name>.",
                "type": "key_value_list",
                "url_values": true,
                "key_placeholder": "gpu-box",
                "value_placeholder": "http://192.168.1.50:11434"
            },
            {
                "key": "default_model",
                "title": "Default Model",
                "description": "Model used when a session doesn't specify ollama:<model>. Must already be pulled on the server it targets (e.g. llama3.1, qwen2.5-coder, or qwen2.5-coder@gpu-box for a named server).",
                "type": "string",
                "placeholder": "llama3.1"
            },
            {
                "key": "request_timeout_secs",
                "title": "Request Timeout (Seconds)",
                "description": "How long to wait for a single Ollama response. Increase if you're running large models on CPU and the first turn times out.",
                "type": "integer",
                "default": 600,
                "min": 1,
                "max": 3600
            },
            {
                "key": "discover_models",
                "title": "Auto-Discover Models",
                "description": "Ask every configured Ollama server which models it has installed (via the OpenAI-compatible /v1/models endpoint) and list them in the model picker automatically. Turn this off to show only the built-in suggestions plus any models you add below.",
                "type": "boolean",
                "default": true
            },
            {
                "key": "enable_tools",
                "title": "Enable Tools",
                "description": "Offer Peckboard's MCP tools (core tools plus any active plugin tools) to the model on every turn, and run the tool calls it makes. Requires a tool-capable model (e.g. llama3.1, qwen2.5-coder); turn this off for models that don't support tools, or to keep sessions chat-only.",
                "type": "boolean",
                "default": true
            },
            {
                "key": "additional_models",
                "title": "Additional Models",
                "description": "Extra model names to add to the picker on top of the auto-discovered list (or the built-in suggestions when discovery is off). Use the exact name as pulled on the server, including any tag (e.g. llama3.1:8b, mistral-small, me/custom-model). Add @<server name> to target one of the additional servers (e.g. llama3.1:8b@gpu-box). Each appears as ollama:<name>.",
                "type": "string_list",
                "item_placeholder": "llama3.1:8b"
            },
            {
                "key": "additional_headers",
                "title": "Additional HTTP Headers",
                "description": "Extra headers attached to every request, to every configured server. Use this for an auth proxy (Authorization: Bearer …). Values are stored encrypted-at-rest only in the sense that they're not echoed back through the API once saved.",
                "type": "key_value_list",
                "secret_values": true,
                "key_placeholder": "Authorization",
                "value_placeholder": "Bearer …"
            }
        ],
    })
    .to_string()
}
