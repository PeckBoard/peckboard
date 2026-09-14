# Peckboard Claude Plugin

Drives sessions via the Claude CLI in stream-json mode. WASM Extism plugin that registers the `claude` AI provider with the static seed catalog (Fable 5, Opus 5 / 4.8 / 4.7 / 4.6, Sonnet 5 / 4.6, Haiku 4.5); send is a stub that emits Started / Text("not implemented") / Completed.

## Build

```bash
./build.sh
# → target/wasm32-unknown-unknown/release/peckboard_claude_plugin.wasm
```

Install via the Peckboard plugin registry (id `claude`), or drop the `.wasm` into `<dataDir>/plugins/claude.wasm` and restart.

Repository: https://github.com/PeckBoard/claude
