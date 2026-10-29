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

Model ids are used verbatim, so Hugging Face-style ids work: `openai-compat:Qwen/Qwen3-32B`. The picker also always lists `openai-compat:default`, which uses the Default Model setting.

<details markdown="1">
<summary>Turn behaviour and failures</summary>

Each turn is a non-streaming `POST /v1/chat/completions`, one request per tool round. The server is stateless, so PeckBoard keeps the conversation history itself; image attachments are sent as `image_url` data-URL parts. `reasoning_content` from reasoning models shows as thinking, and `usage` token counts feed the normal usage pipeline. An HTTP error crashes the turn with the status and the server's error message; interrupts are cooperative and keep the partial answer. There is no limit on tool rounds per turn; a model that makes the exact same tool call three times in a row is treated as stuck, and the turn ends with a warning (send a message to continue).

</details>
