# Peckboard Mock Plugin

Scripted in-process provider for dev/test scenarios. WASM Extism plugin that
registers the `mock` AI provider with the full mock catalog (`doc-review`,
`echo`, `happy-path`, …) so Peckboard sessions can pick `mock:*` model ids.
`provider.send` runs the same scripted `ProviderEvent` sequences as
`src/provider/mock/mod.rs::run_scenario`.

## Build

```bash
./build.sh
# → target/wasm32-unknown-unknown/release/peckboard_mock_plugin.wasm
```

Install via the Peckboard plugin registry (id `mock`), or drop the `.wasm` into
`<dataDir>/plugins/mock.wasm` and restart.

Repository: https://github.com/PeckBoard/mock
