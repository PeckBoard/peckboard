pub mod builtin;
pub mod builtins;
pub mod hooks;
pub mod host;
pub mod manager;
pub mod settings;
pub mod todo_hook;

// Plugin system has two complementary halves:
//
// * `builtin` / `builtins` — first-class Rust plugins compiled into the
//   Peckboard binary. Each declares its [`builtin::Permission`]s, gets
//   them granted at startup, and registers capabilities (currently:
//   AgentProviders) through a `PluginInitContext`. The catalog is read
//   by the Settings UI via `/api/plugins`.
//
// * `manager` (+ `hooks`, `host`, `todo_hook`) — the Extism WASM runtime
//   that loads `.wasm` plugins out of `<dataDir>/plugins/` and dispatches
//   hook calls with cancel/modify semantics. Untouched by the built-in
//   plugin work; the two systems coexist.
