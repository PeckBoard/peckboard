//! `run_background` / `background_status` / `list_background` /
//! `stop_background` — peckboard-managed background processes (see
//! [`crate::background`]). Every task is scoped to the calling session: the
//! status/stop tools refuse a task id that belongs to another session with
//! the same "not found" a bogus id gets.

use std::sync::Arc;

use serde_json::Value;

use super::super::McpToolRegistry;
use super::common_tools::{CommandArgs, auto_approve_for, prompt_for_approval, still_waiting};
use crate::background::{BackgroundRegistry, report};
use crate::service::mcp_server::common_tools::{self, cli};
use crate::service::mcp_server::context::ToolCallContext;

/// Default `lines` for `background_status`.
const DEFAULT_STATUS_LINES: usize = 40;

fn registry(ctx: &ToolCallContext) -> anyhow::Result<Arc<BackgroundRegistry>> {
    ctx.background
        .clone()
        .ok_or_else(|| anyhow::anyhow!("background tasks are unavailable in this context"))
}

fn task_id_arg(args: &Value) -> anyhow::Result<String> {
    args.get("task_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("`task_id` is required"))
}

impl McpToolRegistry {
    /// `run_background` — start an approval-gated command as a background
    /// task. Uses exactly `run_command`'s gate (worker / bypass auto-approve,
    /// persisted "always" grants, else the interactive prompt), with its own
    /// pending-prompt key so the two tools never consume each other's answers.
    pub(crate) async fn handle_run_background(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let registry = registry(ctx)?;
        let CommandArgs {
            command,
            argv,
            timeout,
            reason,
        } = CommandArgs::parse(&args)?;
        let label = args
            .get("label")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        tracing::info!(session_id = %ctx.session_id, command = %command, "MCP tool: run_background");

        let auto_approve = auto_approve_for(ctx).await;
        let db = ctx.db.clone();
        let inv = common_tools::inv_from_ctx(ctx);
        let key = cli::background_pending_key(&ctx.session_id, &command, &argv);
        let cmd = command.clone();
        let av = argv.clone();
        // Validate the program + resolve cwd/env BEFORE asking: the user is
        // never prompted to approve a command that could not run anyway.
        let (prepared, approval) = tokio::task::spawn_blocking(move || {
            let prepared = crate::plugin::host::prepare_exec(&db, &cmd, &inv, false, None)?;
            let approval = cli::decide_approval(&db, &inv, &key, &cmd, &av, auto_approve)?;
            Ok::<_, String>((prepared, approval))
        })
        .await?
        .map_err(|e| anyhow::anyhow!(e))?;

        match approval {
            cli::Approval::Approved(via) => {
                let display = report::display_command(&command, &argv);
                let info = registry
                    .spawn(&ctx.session_id, prepared, argv, label, timeout)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
                Ok(serde_json::json!({
                    "task_id": info.id,
                    "pid": info.pid,
                    "log_path": info.log_path,
                    "label": info.label,
                    "status": info.status,
                    "timeout_secs": info.timeout_secs,
                    "command": display,
                    "approved_via": via,
                    "message": "Started in the background. When it exits you will be notified \
                                automatically in this session with its status and last output \
                                lines \u{2014} do NOT poll or sleep-wait for it; continue with other \
                                work or end your turn. Use background_status to peek at output \
                                or stop_background to stop it.",
                }))
            }
            cli::Approval::Denied(m) => Err(anyhow::anyhow!(m)),
            cli::Approval::StillWaiting(display) => Ok(still_waiting(&display)),
            cli::Approval::NeedsPrompt {
                token,
                display,
                options,
            } => {
                prompt_for_approval(
                    ctx,
                    "Approve starting this command as a background task?",
                    "run_background",
                    reason.as_deref(),
                    &display,
                    &options,
                    &token,
                )
                .await
            }
        }
    }

    /// `background_status` — one task's state plus its last output lines.
    pub(crate) async fn handle_background_status(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let registry = registry(ctx)?;
        let id = task_id_arg(&args)?;
        let lines = args
            .get("lines")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_STATUS_LINES);
        let info = registry
            .get_for_session(&id, &ctx.session_id)
            .ok_or_else(|| anyhow::anyhow!("background task not found: {id}"))?;
        let output = registry.tail(&id, lines).unwrap_or_default();
        Ok(serde_json::json!({ "task": info, "output": output }))
    }

    /// `list_background` — the caller session's tasks (running + finished).
    pub(crate) async fn handle_list_background(
        &self,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let registry = registry(ctx)?;
        Ok(serde_json::json!({ "tasks": registry.list_for_session(&ctx.session_id) }))
    }

    /// `stop_background` — SIGTERM the task's process group (SIGKILL after
    /// the grace period). The STOPPED report still arrives automatically.
    pub(crate) async fn handle_stop_background(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let registry = registry(ctx)?;
        let id = task_id_arg(&args)?;
        if registry.get_for_session(&id, &ctx.session_id).is_none() {
            anyhow::bail!("background task not found: {id}");
        }
        let info = registry.stop(&id).map_err(|e| anyhow::anyhow!(e))?;
        Ok(serde_json::json!({
            "task": info,
            "message": format!(
                "Stop requested: SIGTERM sent to the process group (SIGKILL after {}s). \
                 You will get the STOPPED report automatically.",
                crate::background::STOP_GRACE.as_secs()
            ),
        }))
    }
}
