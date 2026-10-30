use serde_json::{Value, json};

use super::super::McpToolRegistry;
use crate::db::crud::MAX_OPEN_VOICE_TOPICS;
use crate::service::mcp_server::context::ToolCallContext;
use crate::service::voice_relay::VOICE_EXPERT_KIND;
use crate::service::voice_topics;

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

impl McpToolRegistry {
    /// `assistant_topics` — the voice assistant curates the topic ring of
    /// the full-page Assistant view (`service::voice_topics`): `list`,
    /// `add`, `update`, `close`, `focus`. Every change broadcasts the ring
    /// and returns the updated list, compactly.
    ///
    /// Hard-enforced to voice sessions here as well as in `ToolGate`.
    pub(crate) async fn handle_assistant_topics(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let action = str_arg(&args, "action").unwrap_or("list");
        tracing::info!(session_id = %ctx.session_id, action, "MCP tool: assistant_topics");
        let caller = ctx
            .db
            .get_session(&ctx.session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("caller session not found"))?;
        if caller.expert_kind.as_deref() != Some(VOICE_EXPERT_KIND) {
            anyhow::bail!("assistant_topics is only available to the voice assistant session");
        }
        let voice_id = ctx.session_id.as_str();
        let id = || {
            str_arg(&args, "id")
                .ok_or_else(|| anyhow::anyhow!("'{action}' needs the topic id (from list)"))
        };
        let not_found = || anyhow::anyhow!("no open topic with that id; call list to see the ring");
        let changed = match action {
            "list" => false,
            "add" => {
                let kind = str_arg(&args, "kind").unwrap_or("conversation");
                let ref_id = str_arg(&args, "ref_id")
                    .map(str::trim)
                    .filter(|r| !r.is_empty());
                if let (Some(r), "session" | "project") = (ref_id, kind) {
                    voice_topics::check_ref(&ctx.db, voice_id, kind, r).await?;
                }
                let title =
                    str_arg(&args, "title").ok_or_else(|| anyhow::anyhow!("add needs a title"))?;
                ctx.db
                    .add_voice_topic(voice_id, kind, title, str_arg(&args, "summary"), ref_id)
                    .await?;
                true
            }
            "update" => {
                let (title, summary) = (str_arg(&args, "title"), str_arg(&args, "summary"));
                if title.is_none() && summary.is_none() {
                    anyhow::bail!("update needs a title or a summary");
                }
                ctx.db
                    .update_voice_topic(voice_id, id()?, title, summary)
                    .await?
                    .ok_or_else(not_found)?;
                true
            }
            "close" => {
                ctx.db
                    .close_voice_topic(voice_id, id()?)
                    .await?
                    .ok_or_else(not_found)?;
                true
            }
            "focus" => {
                let topic = ctx
                    .db
                    .focus_voice_topic(voice_id, id()?)
                    .await?
                    .ok_or_else(not_found)?;
                voice_topics::focus_gate(&ctx.db, voice_id, &topic).await;
                true
            }
            other => {
                anyhow::bail!("unknown action '{other}'; use list, add, update, close, or focus")
            }
        };
        if changed {
            voice_topics::broadcast(&ctx.db, &ctx.broadcaster, voice_id).await;
        }
        let rows = ctx.db.list_open_voice_topics(voice_id).await?;
        let mut topics = Vec::with_capacity(rows.len());
        for t in &rows {
            let mut v = json!({ "id": t.id, "kind": t.kind, "title": t.title });
            if t.is_current {
                v["current"] = json!(true);
            }
            if let Some(s) = &t.summary {
                v["summary"] = json!(s);
            }
            if let Some(r) = &t.ref_id {
                v["ref_id"] = json!(r);
            }
            if let Some(s) = voice_topics::live_status(&ctx.db, t).await {
                v["status"] = json!(s);
            }
            topics.push(v);
        }
        Ok(json!({
            "open": topics.len(),
            "limit": MAX_OPEN_VOICE_TOPICS,
            "topics": topics,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use crate::db::Db;
    use crate::db::models::{NewFolder, NewSession};
    use crate::service::mcp_server::{McpToolRegistry, ToolCallContext};
    use crate::ws::broadcaster::Broadcaster;

    fn ctx(db: &Db, broadcaster: &Arc<Broadcaster>, session_id: &str) -> ToolCallContext {
        ToolCallContext {
            session_id: session_id.into(),
            project_id: None,
            card_id: None,
            folder_id: "f1".into(),
            db: Arc::new(db.clone()),
            broadcaster: broadcaster.clone(),
            provider_registry: None,
            data_dir: None,
            device_registry: None,
            background: None,
        }
    }

    #[tokio::test]
    async fn assistant_topics_is_voice_only_validates_refs_and_broadcasts() {
        let db = Db::in_memory().unwrap();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "f".into(),
            path: "/tmp".into(),
            created_at: "now".into(),
        })
        .await
        .unwrap();
        for (id, kind) in [("at-voice", Some("voice")), ("at-chat", None)] {
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
        let broadcaster = Broadcaster::new();
        let mut rx = broadcaster.subscribe_all();
        let reg = McpToolRegistry::new();
        let call = |sid: &'static str, args: serde_json::Value| {
            let c = ctx(&db, &broadcaster, sid);
            let reg = &reg;
            async move { reg.handle_tool_call("assistant_topics", args, &c).await }
        };

        let err = call("at-chat", json!({"action": "list"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("voice"), "{err}");

        // A session topic must point at a real session (not the voice one).
        for bad in ["nope", "at-voice"] {
            let err = call(
                "at-voice",
                json!({"action": "add", "kind": "session", "title": "X", "ref_id": bad}),
            )
            .await
            .unwrap_err();
            assert!(err.to_string().contains("no session"), "{err}");
        }

        let added = call(
            "at-voice",
            json!({"action": "add", "kind": "session", "title": "Chat work", "ref_id": "at-chat"}),
        )
        .await
        .unwrap();
        let topic = &added["topics"][0];
        assert_eq!(topic["kind"], "session");
        assert_eq!(topic["status"], "idle");
        let id = topic["id"].as_str().unwrap().to_string();

        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.event_type, "voice-topics");
        assert_eq!(ev.session_id, "at-voice");
        assert_eq!(ev.data["topics"][0]["refId"], "at-chat");
        assert!(ev.data["currentId"].is_null());

        let focused = call("at-voice", json!({"action": "focus", "id": id}))
            .await
            .unwrap();
        assert_eq!(focused["topics"][0]["current"], true);
        assert_eq!(rx.recv().await.unwrap().data["currentId"], id.as_str());

        let closed = call("at-voice", json!({"action": "close", "id": id}))
            .await
            .unwrap();
        assert_eq!(closed["open"], 0);
    }
}
