pub mod hooks;
pub mod host;
pub mod manager;
pub mod todo_hook;

// Plugin system — Extism WASM runtime
//
// Loads .wasm plugins from <dataDir>/plugins/
// Dispatches hook calls with cancel/modify semantics
// Exposes host functions for plugins to call back into Peckboard
