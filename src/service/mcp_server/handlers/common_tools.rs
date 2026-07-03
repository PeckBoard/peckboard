//! Handlers for the native common tools (formerly the `common-tools` plugin).
//!
//! The 14 non-interactive tools run their synchronous, host-touching logic
//! inside `spawn_blocking` (the `*_impl` host functions are blocking); their
//! `Err(String)` is mapped to an MCP tool error. `run_command` is separate: it
//! may need to emit an interactive approval question (async, via the
//! broadcaster) between the two halves of its approval flow.

use serde_json::Value;

use super::super::McpToolRegistry;
use crate::service::mcp_server::common_tools::{self, cli};
use crate::service::mcp_server::context::ToolCallContext;
use crate::service::mcp_server::spawn::emit_plugin_question;

impl McpToolRegistry {
    /// Dispatch one of the 14 non-interactive common tools. Runs the sync tool
    /// body on a blocking thread with a fresh `HostCtx` bound to the caller.
    pub(crate) async fn handle_common_tool(
        &self,
        name: &str,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, tool = %name, "MCP tool: common");
        let db = ctx.db.clone();
        let inv = common_tools::inv_from_ctx(ctx);
        let name = name.to_string();
        let result = tokio::task::spawn_blocking(move || {
            let hc = common_tools::host_bridge::HostCtx { db: &db, inv };
            common_tools::dispatch_sync(&name, args, &hc)
        })
        .await?;
        result.map_err(|e| anyhow::anyhow!(e))
    }

    /// `run_command` — approval-gated arbitrary command execution.
    pub(crate) async fn handle_run_command(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("`command` (bare executable name) is required"))?
            .trim()
            .to_string();
        if command.is_empty() {
            anyhow::bail!("`command` is required");
        }
        let argv: Vec<String> = match args.get("args") {
            Some(Value::Array(a)) => {
                let mut out = Vec::with_capacity(a.len());
                for x in a {
                    match x.as_str() {
                        Some(s) => out.push(s.to_string()),
                        None => anyhow::bail!("each entry in `args` must be a string"),
                    }
                }
                out
            }
            Some(Value::Null) | None => Vec::new(),
            _ => anyhow::bail!("`args` must be an array of strings"),
        };
        let timeout = args.get("timeout_secs").and_then(|v| v.as_u64());

        tracing::info!(session_id = %ctx.session_id, command = %command, "MCP tool: run_command");

        let db = ctx.db.clone();
        let inv = common_tools::inv_from_ctx(ctx);
        let session_id = ctx.session_id.clone();
        let cmd = command.clone();
        let av = argv.clone();
        let decision = tokio::task::spawn_blocking(move || {
            cli::decide(&db, &inv, &session_id, &cmd, &av, timeout)
        })
        .await?
        .map_err(|e| anyhow::anyhow!(e))?;

        match decision {
            cli::Decision::Ran(v) => Ok(v),
            cli::Decision::Denied(m) => Err(anyhow::anyhow!(m)),
            cli::Decision::StillWaiting(display) => Ok(serde_json::json!({
                "status": "awaiting_approval",
                "command": display,
                "message": "Still waiting for the user to approve this command.",
            })),
            cli::Decision::NeedsPrompt {
                token,
                display,
                options,
            } => {
                emit_plugin_question(
                    &ctx.db,
                    &ctx.broadcaster,
                    &ctx.session_id,
                    &format!("Approve running this command?\n\n    {display}"),
                    &options,
                    &token,
                )
                .await?;
                Ok(serde_json::json!({
                    "status": "awaiting_approval",
                    "command": display,
                    "message": format!(
                        "Asked the user to approve running `{display}`. Their answer will resume \
                         this session; then re-call run_command with the same command to proceed."
                    ),
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::McpToolRegistry;
    use crate::service::mcp_server::context::ToolCallContext;
    use std::sync::Arc;

    /// Seed a folder pointing at a real (temp) directory plus a session in it,
    /// and return a `ToolCallContext` scoped to that session/folder. The
    /// `TempDir` is returned so the caller keeps it alive for the test.
    async fn ctx_with_folder() -> (ToolCallContext, tempfile::TempDir) {
        use crate::db::models::{NewFolder, NewSession};
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(crate::db::Db::in_memory().unwrap());
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f-1".into(),
            name: "f-1".into(),
            path: dir.path().to_string_lossy().to_string(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_session(NewSession {
            id: "s-1".into(),
            name: "s-1".into(),
            folder_id: "f-1".into(),
            model: None,
            effort: None,
            is_worker: false,
            project_id: None,
            card_id: None,
            conversation_id: None,
            created_at: ts.clone(),
            last_activity: ts,
            repeating_task_id: None,
            ..Default::default()
        })
        .await
        .unwrap();
        let ctx = ToolCallContext {
            session_id: "s-1".into(),
            project_id: None,
            card_id: None,
            folder_id: "f-1".into(),
            db,
            broadcaster: crate::ws::broadcaster::Broadcaster::new(),
            provider_registry: None,
            data_dir: None,
        };
        (ctx, dir)
    }

    #[tokio::test]
    async fn common_tools_write_read_list_roundtrip() {
        let (ctx, _dir) = ctx_with_folder().await;
        let reg = McpToolRegistry::new();

        // write_file
        let wrote = reg
            .handle_common_tool(
                "write_file",
                serde_json::json!({ "path": "hello.txt", "content": "hi there\nsecond line\n" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(wrote.get("hash").and_then(|h| h.as_str()).is_some());

        // read_file returns the content and a matching hash
        let read = reg
            .handle_common_tool(
                "read_file",
                serde_json::json!({ "path": "hello.txt" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            read["content"].as_str().unwrap().contains("second line"),
            "got: {read}"
        );

        // list_files sees the file we just wrote
        let listed = reg
            .handle_common_tool("list_files", serde_json::json!({}), &ctx)
            .await
            .unwrap();
        let files = listed["files"].as_array().unwrap();
        assert!(
            files
                .iter()
                .any(|f| f["path"].as_str() == Some("hello.txt")),
            "listing missing hello.txt: {listed}"
        );
    }

    #[tokio::test]
    async fn math_tool_through_handler() {
        let (ctx, _dir) = ctx_with_folder().await;
        let reg = McpToolRegistry::new();
        let out = reg
            .handle_common_tool("math", serde_json::json!({ "expression": "6*7" }), &ctx)
            .await
            .unwrap();
        assert_eq!(out["result"].as_f64().unwrap(), 42.0);
    }
}
