# Peckboard Kimi Plugin

Drives sessions through Moonshot AI's Kimi Code CLI in prompt mode. WASM Extism plugin that registers the `kimi` AI provider with the config-default seed model; Send drives a real turn: CLI spawn (or HTTP for Ollama) via host ABI, stream parse, ProviderEvent emit.

## Model Catalog

At runtime this plugin prefers the live CLI/HTTP model list (`discover_models`, default on). Successful discoveries are cached in the plugin data store as a last-good fallback; the compile-time `seed_models()` in `src/models.rs` is the final offline catalog. Refresh checked-in seeds with `scripts/refresh-provider-model-seeds.sh --write` from the repo root.

## Build

```bash
./build.sh
# → target/wasm32-unknown-unknown/release/peckboard_kimi_plugin.wasm
```

Install via the Peckboard plugin registry (id `kimi`), or drop the `.wasm` into `<dataDir>/plugins/kimi.wasm` and restart.

Repository: https://github.com/PeckBoard/kimi
