pub mod builtin;
pub mod crates;
pub mod hooks;
pub mod host;
pub mod manager;
pub mod notify;
pub mod registry;
pub mod session_control_auth;
pub mod settings;
pub mod ssh;
pub mod todo_hook;

// Untrusted plugins are WASM (Extism) from `<dataDir>/plugins/`.
// Trusted first-party plugins (providers + session-control) are crates
// compiled into the binary (`crates` / `BuiltinPluginRegistry`).
