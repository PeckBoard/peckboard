# Peckboard OpenAI-Compatible Plugin

Drives sessions through any server that speaks the OpenAI Chat Completions API (`POST /v1/chat/completions`) — vLLM, llama.cpp server, LM Studio, LocalAI, TGI, SGLang and similar. Registers the `openai-compat` AI provider; Send runs a real turn over HTTP via the host ABI (multi-turn history in the plugin store, Peckboard MCP tools as OpenAI function tools) and emits `ProviderEvent`s.

## Model Catalog

Models come from `GET /v1/models` on the default server and every named server (`discover_models`, default on), plus any Additional Models. Models on a named server appear as `<id>@<server>`. Successful discoveries are cached in the plugin data store as a last-good fallback. The only seed is `openai-compat:default`, which resolves to the Default Model setting (core requires every provider to register at least one model).

## Build

```bash
./build.sh
# → target/wasm32-unknown-unknown/release/peckboard_openai_compat_plugin.wasm
```

Ships compiled into the Peckboard binary as a crate plugin (id `openai-compat`); activate it from the plugin registry.

Repository: https://github.com/PeckBoard/openai-compat
