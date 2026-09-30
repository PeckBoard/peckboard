use serde_json::Value;

use super::super::McpToolRegistry;
use crate::service::mcp_server::context::ToolCallContext;

impl McpToolRegistry {
    /// `reattach_worker` — an orchestrator (a non-worker session, or a user
    /// token, in the card's scope) puts a detached worker session back on
    /// its card. Scope and arguments are checked here; the reattach itself
    /// needs the session manager's lock, so the `mcp` route runs
    /// `worker::reattach::reattach_worker` off the `_reattach_worker` marker
    /// (re-scoping the card to obtain the proof token).
    pub(crate) async fn handle_reattach_worker(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let arg = |k: &str| {
            args.get(k)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let card_id =
            arg("card_id").ok_or_else(|| anyhow::anyhow!("reattach_worker requires 'card_id'"))?;
        let session_id = arg("session_id")
            .ok_or_else(|| anyhow::anyhow!("reattach_worker requires 'session_id'"))?;
        let reason =
            arg("reason").ok_or_else(|| anyhow::anyhow!("reattach_worker requires 'reason'"))?;
        let unblock = args
            .get("unblock")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        // Workers are refused by `ToolGate`; re-check the caller row too.
        if ctx
            .db
            .get_session(&ctx.session_id)
            .await?
            .is_some_and(|s| s.is_worker)
        {
            anyhow::bail!("reattach_worker is for orchestrators, not worker sessions");
        }
        ctx.scope_card(&card_id).await?;

        tracing::info!(
            session_id = %ctx.session_id,
            card_id = %card_id,
            target = %session_id,
            unblock,
            "MCP tool: reattach_worker"
        );
        Ok(serde_json::json!({
            "status": "pending",
            "_reattach_worker": {
                "card_id": card_id,
                "session_id": session_id,
                "unblock": unblock,
                "reason": reason,
            },
        }))
    }
}
