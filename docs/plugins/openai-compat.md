---
title: OpenAI-Compatible Provider
parent: Plugins
nav_order: 19
---

# OpenAI-Compatible Provider

This plugin connects PeckBoard to any endpoint speaking the OpenAI `/v1/chat/completions` protocol — OpenRouter, LM Studio, vLLM, llama.cpp, and anything else with a compatible server. It is the no-code way to add a model source PeckBoard does not ship a provider for; the [Providers]({{ "/providers.html" | relative_url }}) page covers the first-party providers it sits alongside.

Point its settings at the API root (`base_url`, e.g. `https://openrouter.ai/api/v1` or `http://127.0.0.1:1234/v1`), list the model ids to expose, give the provider a display name for the model picker, and supply an `api_key` if the endpoint needs one — local servers accept an empty key. The models then appear in every model picker as `openai-compat:<model>` under your chosen display name, and sessions on them behave like any other: streamed events, usage, and cost all flow through the normal pipeline.

<details markdown="1">
<summary>Hooks and behaviour under failure</summary>

Hooks: `provider.register`, `provider.models`, `provider.send`. As a provider plugin it owns whole agent turns: a turn that times out, traps, or ends without a terminal event is surfaced as a crash by core, so a session never wedges. Interrupts are cooperative — the plugin checks a stop flag between chunks. Changing its settings can re-trigger the approval prompt, since approval is bound to the exact hook and permission set.

</details>
