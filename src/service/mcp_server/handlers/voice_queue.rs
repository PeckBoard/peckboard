use serde_json::{Value, json};

use super::super::McpToolRegistry;
use crate::service::mcp_server::context::ToolCallContext;
use crate::service::voice_gate;
use crate::service::voice_relay::VOICE_EXPERT_KIND;

impl McpToolRegistry {
    /// `voice_queue` — the voice assistant's view of the relays the relay
    /// gate (`service::voice_gate`) is holding back so it stays on one
    /// topic. `list` summarizes them; `next` hands over the next topic
    /// group right now, as the tool result.
    ///
    /// Hard-enforced to voice sessions here as well as in `ToolGate`.
    pub(crate) async fn handle_voice_queue(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("list");
        tracing::info!(session_id = %ctx.session_id, action, "MCP tool: voice_queue");
        let caller = ctx
            .db
            .get_session(&ctx.session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("caller session not found"))?;
        if caller.expert_kind.as_deref() != Some(VOICE_EXPERT_KIND) {
            anyhow::bail!("voice_queue is only available to the voice assistant session");
        }
        match action {
            "list" => {
                let items: Vec<Value> = voice_gate::list(&ctx.session_id)
                    .iter()
                    .map(|h| {
                        json!({
                            "session": h.source_name,
                            "kind": h.kind.as_str(),
                            "summary": h.summary,
                        })
                    })
                    .collect();
                Ok(json!({
                    "count": items.len(),
                    "pending": items,
                    "message": if items.is_empty() {
                        "Nothing is queued."
                    } else {
                        "Queued relays, grouped by session. Call voice_queue with action next to hear the next one."
                    },
                }))
            }
            "next" => {
                let items = voice_gate::take_next(&ctx.db, &ctx.session_id).await;
                if items.is_empty() {
                    return Ok(json!({ "count": 0, "message": "Nothing is queued." }));
                }
                let remaining = voice_gate::list(&ctx.session_id).len();
                let relays: Vec<&str> = items.iter().map(|h| h.text.as_str()).collect();
                Ok(json!({
                    "count": items.len(),
                    "remaining": remaining,
                    "relays": relays,
                    "message": "These are relay messages, handled exactly like [relay] turns: summarize updates, talk questions through and answer them with answer_question.",
                }))
            }
            other => anyhow::bail!("unknown action '{other}'; use list or next"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::db::Db;
    use crate::db::models::{NewFolder, NewSession};
    use crate::service::mcp_server::{McpToolRegistry, ToolCallContext};
    use crate::service::voice_gate::{RelayKind, enqueue};

    fn ctx(db: &Db, session_id: &str) -> ToolCallContext {
        ToolCallContext {
            session_id: session_id.into(),
            project_id: None,
            card_id: None,
            folder_id: "f1".into(),
            db: Arc::new(db.clone()),
            broadcaster: crate::ws::broadcaster::Broadcaster::new(),
            provider_registry: None,
            data_dir: None,
            device_registry: None,
            background: None,
        }
    }

    #[tokio::test]
    async fn voice_queue_lists_and_releases_for_the_voice_session_only() {
        let db = Db::in_memory().unwrap();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "f".into(),
            path: "/tmp".into(),
            created_at: "now".into(),
        })
        .await
        .unwrap();
        let voice_id = uuid::Uuid::new_v4().to_string();
        for (id, kind) in [
            (voice_id.as_str(), Some("voice")),
            ("vq-src", None),
            ("vq-chat", None),
        ] {
            db.create_session(NewSession {
                id: id.into(),
                name: id.into(),
                folder_id: "f1".into(),
                created_at: "now".into(),
                last_activity: "now".into(),
                is_expert: kind.is_some(),
                expert_kind: kind.map(str::to_string),
                user_id: Some("u1".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        let src = db.get_session("vq-src").await.unwrap().unwrap();
        enqueue(
            &db,
            &voice_id,
            &src,
            RelayKind::Update,
            None,
            "[relay] done",
            "done",
        )
        .await;
        let reg = McpToolRegistry::new();

        let err = reg
            .handle_tool_call(
                "voice_queue",
                serde_json::json!({"action": "list"}),
                &ctx(&db, "vq-chat"),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("voice"), "{err}");

        let listed = reg
            .handle_tool_call(
                "voice_queue",
                serde_json::json!({"action": "list"}),
                &ctx(&db, &voice_id),
            )
            .await
            .unwrap();
        assert_eq!(listed["pending"][0]["session"], "vq-src");
        assert_eq!(listed["pending"][0]["kind"], "update");

        let next = reg
            .handle_tool_call(
                "voice_queue",
                serde_json::json!({"action": "next"}),
                &ctx(&db, &voice_id),
            )
            .await
            .unwrap();
        assert_eq!(next["relays"][0], "[relay] done");
        assert_eq!(next["remaining"], 0);
    }
}
