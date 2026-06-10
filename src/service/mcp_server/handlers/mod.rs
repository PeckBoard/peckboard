//! MCP tool handlers, grouped by topic. Every handler is an inherent
//! `async fn` on `McpToolRegistry` — they land on the same struct
//! regardless of which submodule defines them, and the dispatcher in the
//! parent `mod.rs` routes tool names to the right method.

mod ask_expert;
mod cards;
mod experts;
mod folders;
mod misc;
mod pm_expert;
mod projects;
mod repeating_tasks;
mod reports;
mod workers;
