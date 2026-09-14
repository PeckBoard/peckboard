//! Provider-account login helpers (OAuth, device-code, plan usage).
//!
//! These are not `AgentProvider`s. Turns run in WASM plugins. Account
//! rows and the login HTTP surface still live in core so Settings can
//! mint credentials the plugins inherit via the CLI's usual home dir.

pub mod claude_oauth;
pub mod claude_plan_usage;
pub mod claude_token_refresh;
pub mod codex_login;
pub mod grok_login;
pub mod kimi_login;
pub mod login_stash;
