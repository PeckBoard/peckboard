//! Native, always-on worker tools (formerly the `common-tools` WASM plugin).
//!
//! These 15 MCP tools — math, web fetch/search/parse, project file
//! read/write/edit/search/outline, git, run_tests, and the approval-gated
//! run_command — used to live in a WASM plugin that called back into core over
//! the Extism FFI. They now live natively in core: the pure logic is unchanged
//! (ported verbatim), and the FFI calls are replaced by [`host_bridge::HostCtx`],
//! a synchronous shim over the same `crate::plugin::host::*_impl` host functions.
//!
//! The 14 non-interactive tools go through [`dispatch_sync`] (run inside
//! `spawn_blocking` by the handler); `run_command` is handled separately
//! because it may need to emit an interactive question (see [`cli::decide`] and
//! `handlers::common_tools`).

pub mod cli;
pub mod edit;
pub mod exec_tools;
pub mod files;
pub mod host_bridge;
pub mod math;
pub mod outline;
pub mod web;

use serde_json::Value;

use super::context::ToolCallContext;
use crate::plugin::host::InvocationContext;
use host_bridge::HostCtx;

/// The canonical names of every tool this module provides, in the same order
/// as their `McpToolDef`s in `schemas.rs`.
#[allow(dead_code)] // full name list kept for tooling/introspection
pub const TOOL_NAMES: &[&str] = &[
    "math",
    "search_web",
    "fetch_web",
    "web_get_part",
    "parse_web",
    "search_files",
    "list_files",
    "read_file",
    "write_file",
    "edit_file",
    "file_outline",
    "read_symbol",
    "git",
    "run_tests",
    "run_command",
];

/// Build the trusted invocation context the file/exec/store host impls resolve
/// the caller's folder from. `authority=false`: these are MCP tool calls, so
/// the per-folder visibility floor applies.
pub fn inv_from_ctx(ctx: &ToolCallContext) -> InvocationContext {
    InvocationContext {
        session_id: Some(ctx.session_id.clone()),
        project_id: ctx.project_id.clone(),
        folder_id: Some(ctx.folder_id.clone()),
        authority: false,
    }
}

/// Synchronous dispatcher for the 14 non-interactive tools. `run_command` is
/// NOT handled here (it is async / interactive — see `handle_run_command`).
pub fn dispatch_sync(name: &str, args: Value, ctx: &HostCtx) -> Result<Value, String> {
    match name {
        "math" => math::eval_tool(args),
        "search_web" => web::search_web_tool(args, ctx),
        "fetch_web" => web::fetch_web_tool(args, ctx),
        "web_get_part" => web::web_get_part_tool(args, ctx),
        "parse_web" => web::parse_web_tool(args, ctx),
        "search_files" => files::search_files_tool(args, ctx),
        "list_files" => files::list_files_tool(args, ctx),
        "read_file" => files::read_file_tool(args, ctx),
        "write_file" => files::write_file_tool(args, ctx),
        "edit_file" => edit::edit_file_tool(args, ctx),
        "file_outline" => outline::file_outline_tool(args, ctx),
        "read_symbol" => outline::read_symbol_tool(args, ctx),
        "git" => exec_tools::git_tool(args, ctx),
        "run_tests" => exec_tools::run_tests_tool(args, ctx),
        other => Err(format!("unknown common tool: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_ctx_hostctx(db: &crate::db::Db) -> HostCtx<'_> {
        HostCtx {
            db,
            inv: InvocationContext::default(),
        }
    }

    #[test]
    fn dispatch_math_evaluates_expression() {
        let db = crate::db::Db::in_memory().unwrap();
        let hc = no_ctx_hostctx(&db);
        let out = dispatch_sync("math", serde_json::json!({ "expression": "2+3" }), &hc).unwrap();
        assert_eq!(out["result"].as_f64().unwrap(), 5.0);
    }

    #[test]
    fn dispatch_unknown_tool_errors() {
        let db = crate::db::Db::in_memory().unwrap();
        let hc = no_ctx_hostctx(&db);
        let err = dispatch_sync("nope", serde_json::json!({}), &hc).unwrap_err();
        assert!(err.contains("unknown common tool"), "got: {err}");
    }
}
