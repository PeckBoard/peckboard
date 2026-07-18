//! MCP tool handlers, grouped by topic. Every handler is an inherent
//! `async fn` on `McpToolRegistry` — they land on the same struct
//! regardless of which submodule defines them, and the dispatcher in the
//! parent `mod.rs` routes tool names to the right method.

mod browser;
mod cards;
mod common_tools;
mod folders;
mod misc;
mod model_control;
mod plans;
mod projects;
mod repeating_tasks;
mod reports;
mod subagents;
mod workers;

pub use model_control::autoswitch_enabled;
