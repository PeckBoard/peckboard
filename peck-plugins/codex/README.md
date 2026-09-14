# Peckboard Codex Plugin

Drives sessions via the OpenAI Codex CLI (`codex exec --json`). Sign in with ChatGPT under Settings → Codex Accounts. WASM Extism plugin that registers the `codex` AI provider with the built-in seed catalog (`gpt-5.6-luna`, `gpt-5.6-terra`, …); Send drives a real turn: CLI spawn (or HTTP for Ollama) via host ABI, stream parse, ProviderEvent emit.

## Build

```bash
./build.sh
# → target/wasm32-unknown-unknown/release/peckboard_codex_plugin.wasm
```

Install via the Peckboard plugin registry (id `codex`), or drop the `.wasm` into `<dataDir>/plugins/codex.wasm` and restart.

Repository: https://github.com/PeckBoard/codex
