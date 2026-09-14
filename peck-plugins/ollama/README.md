# Peckboard Ollama Plugin

Drives sessions through an Ollama server's /api/chat endpoint. WASM Extism plugin that registers the `ollama` AI provider with the built-in seed catalog (`llama3.1`, `llama3.2`, `qwen2.5-coder`); Send drives a real turn: CLI spawn (or HTTP for Ollama) via host ABI, stream parse, ProviderEvent emit.

## Build

```bash
./build.sh
# → target/wasm32-unknown-unknown/release/peckboard_ollama_plugin.wasm
```

Install via the Peckboard plugin registry (id `ollama`), or drop the `.wasm` into `<dataDir>/plugins/ollama.wasm` and restart.

Repository: https://github.com/PeckBoard/ollama
