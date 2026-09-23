# Peckboard Claude Plugin

Drives sessions via the Claude CLI in stream-json mode. WASM Extism plugin that registers the `claude` AI provider with the static seed catalog (Fable 5.1 / 5, Opus 5.5 / 5 / 4.8 / 4.7 / 4.6, Sonnet 5 / 4.6, Haiku 4.5); Send drives a real turn: CLI spawn (or HTTP for Ollama) via host ABI, stream parse, ProviderEvent emit.

## Model Catalog

At runtime this plugin prefers the live CLI/HTTP model list (`discover_models`, default on). Successful discoveries are cached in the plugin data store as a last-good fallback; the compile-time `seed_models()` in `src/models.rs` is the final offline catalog. Refresh checked-in seeds with `scripts/refresh-provider-model-seeds.sh --write` from the repo root.

## Build

```bash
./build.sh
# → target/wasm32-unknown-unknown/release/peckboard_claude_plugin.wasm
```

Install via the Peckboard plugin registry (id `claude`), or drop the `.wasm` into `<dataDir>/plugins/claude.wasm` and restart.

Repository: https://github.com/PeckBoard/claude
