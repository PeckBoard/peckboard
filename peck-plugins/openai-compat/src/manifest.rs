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
                "key": "base_url",
                "title": "Default Server URL",
                "description": "Base URL of your default OpenAI-compatible server (vLLM, llama.cpp server, LM Studio, LocalAI, TGI, SGLang, …), with or without the trailing /v1 — e.g. http://localhost:8000 for vLLM, http://localhost:1234 for LM Studio. Models on this server appear under their bare id.",
                "required": true,
                "type": "url",
                "default": "http://localhost:8000",
                "placeholder": "http://localhost:8000"
            },
            {
                "key": "api_key",
                "title": "API Key",
                "description": "Sent as Authorization: Bearer <key> on every request (chat and model discovery), to every configured server. Leave empty for servers that don't check a key.",
                "type": "string",
                "secret": true,
                "placeholder": "sk-…"
            },
            {
                "key": "servers",
                "title": "Additional Servers",
                "description": "More OpenAI-compatible servers, as name → base URL pairs. Models on a named server show up as model@name in the picker and can be selected as openai-compat:<model>@<name>.",
                "type": "key_value_list",
                "url_values": true,
                "key_placeholder": "gpu-box",
                "value_placeholder": "http://192.168.1.50:8000"
            },
            {
                "key": "default_model",
                "title": "Default Model",
                "description": "Model used when a session doesn't specify openai-compat:<model>. Use the id the server reports in /v1/models (e.g. Qwen/Qwen3-32B, or Qwen/Qwen3-32B@gpu-box for a named server).",
                "type": "string",
                "placeholder": "Qwen/Qwen3-32B"
            },
            {
                "key": "request_timeout_secs",
                "title": "Request Timeout (Seconds)",
                "description": "How long to wait for a single chat completion. Increase for large models on slow hardware.",
                "type": "integer",
                "default": 600,
                "min": 1,
                "max": 3600
            },
            {
                "key": "discover_models",
                "title": "Auto-Discover Models",
                "description": "Ask every configured server which models it serves (GET /v1/models) and list them in the model picker automatically. Turn this off to show only the models you add below.",
                "type": "boolean",
                "default": true
            },
            {
                "key": "enable_tools",
                "title": "Enable Tools",
                "description": "Offer Peckboard's MCP tools (core tools plus any active plugin tools) to the model as OpenAI function tools on every turn, and run the tool calls it makes. Needs a server and model with tool calling enabled (e.g. vLLM with --enable-auto-tool-choice); turn this off for models that don't support tools, or to keep sessions chat-only.",
                "type": "boolean",
                "default": true
            },
            {
                "key": "additional_models",
                "title": "Additional Models",
                "description": "Extra model ids to add to the picker on top of the auto-discovered list. Use the exact id the server expects (e.g. Qwen/Qwen3-32B, llama-3.1-8b-instruct). Add @<server name> to target one of the additional servers (e.g. Qwen/Qwen3-32B@gpu-box). Each appears as openai-compat:<id>.",
                "type": "string_list",
                "item_placeholder": "Qwen/Qwen3-32B"
            },
            {
                "key": "additional_headers",
                "title": "Additional HTTP Headers",
                "description": "Extra headers attached to every request, to every configured server — e.g. for an auth proxy. An Authorization header here overrides the API Key. Values are not echoed back through the API once saved.",
                "type": "key_value_list",
                "secret_values": true,
                "key_placeholder": "X-Api-Key",
                "value_placeholder": "…"
            }
        ],
    })
    .to_string()
}
