---
title: OpenAI-Compatible Provider
parent: Plugins
nav_order: 19
---

# OpenAI-Compatible Provider

The OpenAI-Compatible provider (`openai-compat`) connects PeckBoard to any server speaking the OpenAI Chat Completions API — vLLM, llama.cpp server, LM Studio, LocalAI, TGI, SGLang, OpenRouter, and anything else with a compatible `/v1/chat/completions` endpoint. It ships compiled into PeckBoard as a first-party provider; activate it from Settings → Plugin Registry and configure it under Settings → Providers & Accounts. The [Providers]({{ "/providers.html" | relative_url }}) page covers the other first-party providers it sits alongside.

## Settings

- **Default Server URL** — the server root, with or without the trailing `/v1` (default `http://localhost:8000`). Models on this server appear under their bare id.
- **API Key** — optional; sent as `Authorization: Bearer <key>` on every chat and discovery request. Stored secret: never echoed back.
- **Additional Servers** — named extra servers; their models appear as `model@name` and are selected as `openai-compat:<model>@<name>`.
- **Auto-Discover Models** — lists every server's `GET /v1/models` in the model picker (on by default). The last successful list is cached, so a server that is briefly down keeps its models in the picker.
- **Additional Models**, **Default Model**, **Request Timeout**, **Additional HTTP Headers** — as for Ollama.
- **Enable Tools** — offers PeckBoard's MCP tools as OpenAI function tools. A server that rejects tools for a model (e.g. vLLM without `--enable-auto-tool-choice`) gets the turn retried once without them.
- **Tool Set** — All tools (default) sends every PeckBoard tool, about 200 definitions and roughly 45k prompt tokens per request. Core coding tools sends a short coding set instead (file read/search/edit, `run_command`, `run_tests`, `git`, `graphify_*`, browser, `ask_user`, card updates, background tasks, `fetch_url`, `search_web`), plus any names under **Extra Tools**; tools the session doesn't have are skipped. Either way the tool list is sorted by name, so it is byte-identical across requests and stays in the server's prefix cache.
- **Thinking** — Auto (default), On, or Off. Sets `chat_template_kwargs.enable_thinking`: Auto sends `true` only for models whose id contains `qwen3` (Qwen3 templates render an empty think block unless it's explicitly on, so the model never reasons) and nothing for other models; On and Off always send `true` / `false`.
- **Temperature**, **Top P**, **Top K**, **Min P**, **Presence Penalty**, **Max Tokens** — optional sampling settings, each sent only when set (empty, or `0` for Top K / Max Tokens, leaves the server default). Temperature is 0–2, Top P above 0 up to 1, Min P 0–1, Presence Penalty −2 to 2; an out-of-range value is skipped with a warning on the turn.
- **Max Tool Result Size** — longest tool result sent to the model (default 24,000 characters; `0` = no limit). A longer one is cut to its start and end around a `[… N chars omitted: narrow the request …]` note; the chat still shows the full output.

Model ids are used verbatim, so Hugging Face-style ids work: `openai-compat:Qwen/Qwen3-32B`. The picker also always lists `openai-compat:default`, which uses the Default Model setting.

<details markdown="1">
<summary>Turn behaviour and failures</summary>

Each turn is a non-streaming `POST /v1/chat/completions`, one request per tool round. The server is stateless, so PeckBoard keeps the conversation history itself; image attachments are sent as `image_url` data-URL parts. `reasoning_content` from reasoning models shows as thinking, and so does reasoning that arrives inline in the reply when the server runs without `--reasoning-parser` (`…</think>answer`): everything before the last `</think>` is thinking, and only the answer is shown and kept in the history. Tool results go back to the model as plain text — string results as-is, MCP text parts joined, anything else as pretty-printed JSON. `usage` token counts feed the normal usage pipeline. An HTTP error crashes the turn with the status and the server's error message; interrupts are cooperative and keep the partial answer. A reply cut off by the token limit (`finish_reason: length`) shows a warning.

There is no limit on tool rounds per turn; a model that makes the exact same tool call three times in a row is treated as stuck, and the turn ends with a warning (send a message to continue). With tools on, a reply that ends by announcing a step without making the call ("Let me check the logs.", or a final line ending in `:`) gets a short "continue" message and another round, at most twice per turn.

The stored history holds up to 160 messages. Past that it's trimmed to about 96 in one go, starting at a user message, rather than one message per turn — so the prompt prefix stays identical for many turns and the server's prefix cache (e.g. vLLM's) keeps hitting instead of re-processing the whole context every turn. If the server still rejects a request as longer than the model's context window, about half of the older turns are dropped (with a "History trimmed" warning) and the round is retried once; a second overflow crashes the turn as before.

Every openai-compat session's default base prompt starts with a short set of operating rules for local models (act instead of announcing, edit files safely with `edit_file`, treat errors as failures, keep tool output small), ahead of the shared working-style rules. Replace it under Settings → Providers & Accounts like any provider's base prompt.

</details>
