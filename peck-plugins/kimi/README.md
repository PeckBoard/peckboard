# Peckboard Kimi Plugin

Drives sessions through Moonshot AI's Kimi Code CLI in prompt mode. WASM Extism plugin that registers the `kimi` AI provider with the config-default seed model; send is a stub that emits Started / Text("not implemented") / Completed.

## Build

```bash
./build.sh
# → target/wasm32-unknown-unknown/release/peckboard_kimi_plugin.wasm
```

Install via the Peckboard plugin registry (id `kimi`), or drop the `.wasm` into `<dataDir>/plugins/kimi.wasm` and restart.

Repository: https://github.com/PeckBoard/kimi
