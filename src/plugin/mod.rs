pub mod builtin;
pub mod hooks;
pub mod host;
pub mod manager;
pub mod notify;
pub mod registry;
pub mod session_control_auth;
pub mod settings;
pub mod ssh;
pub mod todo_hook;

// Plugins are WASM (Extism), loaded from `<dataDir>/plugins/`.
// First-party AI providers ship as wasm in `peck-plugins-wasm/` and are
// extracted + auto-approved at boot. There is no compiled-in AgentProvider.
