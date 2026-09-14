# Peckboard Grok Plugin

Drives sessions via the Grok CLI in streaming-json mode. WASM Extism plugin that registers the `grok` AI provider with the built-in seed catalog (`grok-4.6`, `grok-4.5`); Send drives a real turn: CLI spawn (or HTTP for Ollama) via host ABI, stream parse, ProviderEvent emit.

## Build

```bash
./build.sh
# → target/wasm32-unknown-unknown/release/peckboard_grok_plugin.wasm
```

Install via the Peckboard plugin registry (id `grok`), or drop the `.wasm` into `<dataDir>/plugins/grok.wasm` and restart.

Repository: https://github.com/PeckBoard/grok
