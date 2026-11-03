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
                "description": "How long to wait for a single chat completion (requests are non-streaming, so this covers the whole reply). On slow local hardware a round that writes a whole file can take more than 10 minutes.",
                "type": "integer",
                "default": 1800,
                "min": 1,
                "max": 7200
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
                "description": "Offer Peckboard's MCP tools (core tools plus any active plugin tools) to the model as OpenAI function tools on every turn, and run the tool calls it makes. A reply that announces a next step (\"Let me check…\") but makes no call is nudged to continue, at most twice per turn. Needs a server and model with tool calling enabled (e.g. vLLM with --enable-auto-tool-choice); turn this off for models that don't support tools, or to keep sessions chat-only.",
                "type": "boolean",
                "default": true
            },
            {
                "key": "tool_set",
                "title": "Tool Set",
                "description": "Which tools to offer when Enable Tools is on. All tools sends every Peckboard tool (around 200 definitions, roughly 45k prompt tokens per request). Core coding tools sends a short list for coding work (files, search, edit, run_command, run_tests, git, browser, ask_user, cards, background tasks, web fetch/search) plus any Extra Tools: much faster prefill, and smaller models pick tools more reliably.",
                "type": "enum",
                "options": [
                    { "value": "all", "label": "All tools" },
                    { "value": "core", "label": "Core coding tools" }
                ],
                "default": "all"
            },
            {
                "key": "extra_tools",
                "title": "Extra Tools",
                "description": "Tool names to offer on top of Core coding tools (e.g. create_card, spawn_subagent). Ignored with All tools. Names the session doesn't have are skipped.",
                "type": "string_list",
                "item_placeholder": "create_card"
            },
            {
                "key": "thinking",
                "title": "Thinking",
                "description": "Whether to ask the chat template for a reasoning block (chat_template_kwargs.enable_thinking). Auto turns it on for Qwen3 models — their templates otherwise render an empty think block, so the model never reasons — and sends nothing for other models. Reasoning that arrives inline in the reply (no --reasoning-parser on the server) is split off and shown as thinking either way.",
                "type": "enum",
                "options": [
                    { "value": "auto", "label": "Auto (on for Qwen3 models)" },
                    { "value": "on", "label": "On" },
                    { "value": "off", "label": "Off" }
                ],
                "default": "auto"
            },
            {
                "key": "thinking_budget",
                "title": "Thinking Budget (Tokens)",
                "description": "Cap on tokens per round while thinking is on (the server can't cap reasoning alone, so this covers the answer too). A round that hits it is re-asked with its reasoning closed off and no cap (vLLM's continue_final_message), so the model acts on what it worked out instead of reasoning until the request times out, and a file write cut at the cap is written again in full. Servers that don't support this fall back to the cut-off reply with a warning. 0 = no cap.",
                "type": "integer",
                "default": 8192,
                "min": 0,
                "max": 262144
            },
            {
                "key": "temperature",
                "title": "Temperature",
                "description": "Sampling temperature, 0 to 2. Leave empty for the default: 0.6 with thinking on and 0.7 with it off for Qwen3 models (Qwen's recommendation; some Qwen3 finetunes ship 1.0, which garbles long file writes), the server's default for other models.",
                "type": "string",
                "placeholder": "0.6"
            },
            {
                "key": "top_p",
                "title": "Top P",
                "description": "Nucleus sampling, above 0 up to 1. Leave empty for the server's default.",
                "type": "string",
                "placeholder": "0.95"
            },
            {
                "key": "top_k",
                "title": "Top K",
                "description": "Sample from the K most likely tokens. 0 leaves it to the server's default.",
                "type": "integer",
                "default": 0,
                "min": 0,
                "max": 1000
            },
            {
                "key": "min_p",
                "title": "Min P",
                "description": "Minimum token probability relative to the most likely one, 0 to 1. Leave empty for the server's default.",
                "type": "string",
                "placeholder": "0"
            },
            {
                "key": "presence_penalty",
                "title": "Presence Penalty",
                "description": "Penalise tokens already in the reply, -2 to 2; 1 to 1.5 curbs repetition loops in quantised models. Leave empty for the server's default.",
                "type": "string",
                "placeholder": "1.5"
            },
            {
                "key": "max_tokens",
                "title": "Max Tokens",
                "description": "Cap on tokens generated per reply (reasoning included). 0 leaves it to the server, which allows up to the rest of the context window. A reply that hits the cap is flagged with a warning.",
                "type": "integer",
                "default": 0,
                "min": 0,
                "max": 1048576
            },
            {
                "key": "send_tool_images",
                "title": "Send Tool Images",
                "description": "Send images returned by tools, such as browser screenshots, to the model as image inputs. Turn off for text-only models.",
                "type": "boolean",
                "default": true
            },
            {
                "key": "max_history_tokens",
                "title": "Max History Tokens",
                "description": "Estimated-token budget for the conversation history sent each turn (tool definitions and the base prompt come on top). Past it, the oldest turns are dropped down to 60% of the budget in one go, so the prompt prefix stays cacheable. Local models get noticeably worse at long contexts; raise this for models that handle them well. 0 = no budget.",
                "type": "integer",
                "default": 40000,
                "min": 0,
                "max": 2000000
            },
            {
                "key": "compact_turn_tokens",
                "title": "Compact Long Turns At (Tokens)",
                "description": "Estimated-token size of the conversation (base prompt, history and the current turn; tool definitions come on top) past which a long turn is compacted: in all but its last 4 tool rounds, file contents the model already wrote, long tool results and old reasoning are replaced with short notes. Keeps multi-hour builds from filling the context window. 0 = never compact.",
                "type": "integer",
                "default": 48000,
                "min": 0,
                "max": 2000000
            },
            {
                "key": "max_tool_result_chars",
                "title": "Max Tool Result Size (Characters)",
                "description": "Longest tool result sent to the model. A longer one is cut to its start and end, with a note in the middle telling the model to narrow the request (line ranges, filters, tail). The chat still shows the full output. 0 means no limit.",
                "type": "integer",
                "default": 24000,
                "min": 0,
                "max": 1000000
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
