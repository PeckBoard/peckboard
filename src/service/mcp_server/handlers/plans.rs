use serde_json::Value;

use super::super::McpToolRegistry;
use crate::service::mcp_server::context::ToolCallContext;

impl McpToolRegistry {
    /// Save (or revise) the calling session's plan. Restricted to thinking
    /// (reasoning) models — planning on a non-thinking model is refused to
    /// avoid hallucinated designs. Available to workers and chats. The plan
    /// lives in the `plans` table, so it survives model switches, agent
    /// termination, and `clear_session`.
    pub(crate) async fn handle_propose_plan(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let markdown = args
            .get("markdown")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("propose_plan requires non-empty 'markdown'"))?;
        let title = args
            .get("title")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Plan");

        let session = ctx
            .db
            .get_session(&ctx.session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("session not found"))?;

        // Thinking-model gate. An unknown or missing model resolves to
        // non-thinking, so planning is refused rather than risked.
        let is_thinking = match (&session.model, &ctx.provider_registry) {
            (Some(model), Some(reg)) => reg.is_thinking_model(model).await,
            _ => false,
        };
        if !is_thinking {
            anyhow::bail!(
                "propose_plan is restricted to thinking (reasoning) models. Switch to a \
                 thinking model (e.g. via switch_session_model) before proposing a plan."
            );
        }

        let plan = ctx
            .db
            .upsert_plan(
                &ctx.session_id,
                ctx.card_id.as_deref(),
                ctx.project_id.as_deref(),
                title,
                markdown,
            )
            .await?;

        let event_data = serde_json::json!({
            "planId": plan.id,
            "title": plan.title,
            "version": plan.version,
        });
        let event = ctx
            .db
            .append_event(&ctx.session_id, "plan-proposed", event_data.clone())
            .await?;
        ctx.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "event".into(),
            session_id: ctx.session_id.clone(),
            data: serde_json::json!({
                "id": event.id,
                "seq": event.seq,
                "ts": event.ts,
                "kind": "plan-proposed",
                "data": event_data,
            }),
        });
        // Live-update the kanban so the card's menu enables its Plan item.
        if let Some(pid) = ctx.project_id.as_deref() {
            ctx.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
                event_type: "plan-update".into(),
                session_id: pid.to_string(),
                data: serde_json::json!({
                    "planId": plan.id,
                    "cardId": ctx.card_id,
                    "sessionId": ctx.session_id,
                }),
            });
        }

        Ok(serde_json::json!({
            "status": "ok",
            "plan_id": plan.id,
            "version": plan.version,
            "note": "Plan saved. It is visible from the 3-dots menu and persists across \
                     model switches, termination, and session clears."
        }))
    }
}
