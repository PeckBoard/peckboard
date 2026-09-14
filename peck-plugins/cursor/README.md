# Peckboard Cursor Plugin

Drives sessions through the cursor-agent CLI in print mode. WASM Extism plugin that registers the `cursor` AI provider with the built-in seed catalog (`auto`, Composer, Opus/Sonnet thinking, …); send is a stub that emits Started / Text("not implemented") / Completed.

## Build

```bash
./build.sh
# → target/wasm32-unknown-unknown/release/peckboard_cursor_plugin.wasm
```

Install via the Peckboard plugin registry (id `cursor`), or drop the `.wasm` into `<dataDir>/plugins/cursor.wasm` and restart.

Repository: https://github.com/PeckBoard/cursor
