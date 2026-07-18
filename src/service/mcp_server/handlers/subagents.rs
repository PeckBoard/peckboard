use serde_json::Value;

use super::super::McpToolRegistry;
use crate::service::mcp_server::context::ToolCallContext;

impl McpToolRegistry {
    /// `spawn_subagent` — create a child session (works on every provider,
    /// unlike Claude's native Task tool) that runs a task and has its final
    /// message posted back to the caller automatically. Row creation and
    /// prompt persistence happen here; the `mcp` route dispatches the
    /// child's first turn off the `_dispatch_session` marker (it holds the
    /// `AppState`), and the completion listener in `main.rs` reports the
    /// result back via `crate::subagent::handle_subagent_done`.
    pub(crate) async fn handle_spawn_subagent(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("spawn_subagent requires 'prompt'"))?;
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("spawn_subagent requires 'name'"))?;

        tracing::info!(session_id = %ctx.session_id, name, "MCP tool: spawn_subagent");

        let caller = ctx
            .db
            .get_session(&ctx.session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("caller session not found"))?;

        // Depth 1 only: a subagent may not spawn subagents (no runaway
        // trees). The pre-hatcher allowlist already blocks this tool for
        // pre-hatcher sessions at the route layer.
        if caller.parent_session_id.is_some() {
            anyhow::bail!(
                "subagents cannot spawn subagents; return your findings and let the parent fan out"
            );
        }

        let active = ctx.db.count_active_subagents(&ctx.session_id).await?;
        if active >= crate::subagent::MAX_CONCURRENT_SUBAGENTS {
            anyhow::bail!(
                "subagent limit reached ({active} in flight, max {}). Results arrive \
                 automatically as they finish; peek with list_sessions / read_worker_session.",
                crate::subagent::MAX_CONCURRENT_SUBAGENTS
            );
        }

        // Model/effort: explicit args win, else inherit the caller's.
        let model = args
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| caller.model.clone());
        let effort = args
            .get("effort")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| caller.effort.clone());

        // Optional library system prompt for the child (research/review/…).
        let resolved = ctx
            .db
            .resolve_system_prompt(args.get("system_prompt_name").and_then(|v| v.as_str()))
            .await
            .map_err(|e| anyhow::anyhow!("spawn_subagent: {e}"))?;
        let (system_prompt_name, system_prompt) = match resolved {
            Some((n, b)) => (Some(n), Some(b)),
            None => (None, None),
        };

        let now = chrono::Utc::now().to_rfc3339();
        let child = ctx
            .db
            .create_session(crate::db::models::NewSession {
                id: uuid::Uuid::new_v4().to_string(),
                name: format!("{}{name}", crate::subagent::SUBAGENT_NAME_PREFIX),
                folder_id: caller.folder_id.clone(),
                model,
                effort,
                is_worker: false,
                project_id: caller.project_id.clone(),
                created_at: now.clone(),
                last_activity: now,
                is_expert: true,
                expert_kind: Some(crate::subagent::SUBAGENT_EXPERT_KIND.to_string()),
                user_id: caller.user_id.clone(),
                system_prompt,
                system_prompt_name,
                parent_session_id: Some(ctx.session_id.clone()),
                ..Default::default()
            })
            .await?;

        // Persist the first turn; the route's marker handler drives the agent.
        let full_prompt = crate::subagent::build_subagent_prompt(name, &ctx.session_id, prompt);
        ctx.db
            .append_event(
                &child.id,
                "user",
                serde_json::json!({ "text": &full_prompt, "source": "subagent-spawn" }),
            )
            .await?;

        Ok(serde_json::json!({
            "status": "ok",
            "subagent_session_id": child.id,
            "note": "Runs in the background; its final message is posted into this session automatically when it finishes. Keep working meanwhile; peek with read_worker_session.",
            "_dispatch_session": { "session_id": child.id, "text": full_prompt },
        }))
    }
}
