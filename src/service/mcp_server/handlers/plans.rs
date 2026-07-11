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

    /// Delete the calling session's plan — the complement of `propose_plan`,
    /// for once the implementation has been verified against the plan (or the
    /// user discards it). Appends a `plan-deleted` event so open chat views
    /// re-fetch the session's plan id and disable their Plan menu item.
    pub(crate) async fn handle_delete_plan(
        &self,
        _args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let plan = ctx
            .db
            .get_plan_for_session(&ctx.session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("this session has no plan to delete"))?;
        ctx.db.delete_plan(&plan.id).await?;

        let event_data = serde_json::json!({
            "planId": plan.id,
            "title": plan.title,
        });
        let event = ctx
            .db
            .append_event(&ctx.session_id, "plan-deleted", event_data.clone())
            .await?;
        ctx.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "event".into(),
            session_id: ctx.session_id.clone(),
            data: serde_json::json!({
                "id": event.id,
                "seq": event.seq,
                "ts": event.ts,
                "kind": "plan-deleted",
                "data": event_data,
            }),
        });
        // Live-update the kanban so the card's menu disables its Plan item.
        if let Some(pid) = ctx.project_id.as_deref() {
            ctx.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
                event_type: "plan-update".into(),
                session_id: pid.to_string(),
                data: serde_json::json!({
                    "planId": serde_json::Value::Null,
                    "cardId": ctx.card_id,
                    "sessionId": ctx.session_id,
                }),
            });
        }

        Ok(serde_json::json!({
            "status": "ok",
            "deleted_plan_id": plan.id,
            "note": "Plan deleted."
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::super::McpToolRegistry;
    use crate::db::models::{NewFolder, NewSession};
    use crate::service::mcp_server::context::ToolCallContext;

    /// In-memory DB with the folder + session rows the `plan-deleted`
    /// event's FK needs, plus a ToolCallContext for that session.
    async fn ctx() -> (ToolCallContext, Arc<crate::db::Db>) {
        let db = Arc::new(crate::db::Db::in_memory().unwrap());
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_session(NewSession {
            id: "s1".into(),
            name: "S".into(),
            folder_id: "f1".into(),
            model: None,
            effort: None,
            is_worker: false,
            project_id: None,
            card_id: None,
            conversation_id: None,
            created_at: ts.clone(),
            last_activity: ts,
            ..Default::default()
        })
        .await
        .unwrap();
        let ctx = ToolCallContext {
            session_id: "s1".into(),
            project_id: None,
            card_id: None,
            db: db.clone(),
            broadcaster: crate::ws::broadcaster::Broadcaster::new(),
            provider_registry: None,
            data_dir: None,
            folder_id: "f1".into(),
        };
        (ctx, db)
    }

    #[tokio::test]
    async fn delete_plan_removes_the_sessions_plan_and_logs_an_event() {
        let (ctx, db) = ctx().await;
        db.upsert_plan("s1", None, None, "Plan", "# steps")
            .await
            .unwrap();
        let out = McpToolRegistry::new()
            .handle_delete_plan(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert_eq!(out["status"], "ok", "got: {out}");
        assert!(db.get_plan_for_session("s1").await.unwrap().is_none());
        let events = db.list_events_by_session("s1", None).await.unwrap();
        assert!(
            events.iter().any(|e| e.kind == "plan-deleted"),
            "expected a plan-deleted event"
        );
    }

    #[tokio::test]
    async fn delete_plan_without_a_plan_errors() {
        let (ctx, _db) = ctx().await;
        let err = McpToolRegistry::new()
            .handle_delete_plan(serde_json::json!({}), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no plan"), "got: {err}");
    }
}
