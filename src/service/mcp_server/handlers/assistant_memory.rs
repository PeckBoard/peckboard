use chrono::Utc;
use serde_json::{Value, json};

use super::super::McpToolRegistry;
use crate::service::assistant_memory::{self, TopicRef};
use crate::service::assistant_settings;
use crate::service::mcp_server::context::ToolCallContext;
use crate::service::voice_relay::VOICE_EXPERT_KIND;

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

impl McpToolRegistry {
    /// `assistant_memory` — the voice assistant's temporary memory
    /// (`service::assistant_memory`): `list`, `add`, `forget`. Writes never
    /// touch the stored prompt (its Recent Memory snapshot only moves at a
    /// context clear), so they don't restart the agent.
    ///
    /// Hard-enforced to voice sessions here as well as in `ToolGate`.
    pub(crate) async fn handle_assistant_memory(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let action = str_arg(&args, "action").unwrap_or("list");
        tracing::info!(session_id = %ctx.session_id, action, "MCP tool: assistant_memory");
        let caller = ctx
            .db
            .get_session(&ctx.session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("caller session not found"))?;
        if caller.expert_kind.as_deref() != Some(VOICE_EXPERT_KIND) {
            anyhow::bail!("assistant_memory is only available to the voice assistant session");
        }
        let voice_id = ctx.session_id.as_str();
        let settings = assistant_settings::load(&ctx.db).await;
        let now = Utc::now();
        let mut out = json!({});
        match action {
            "list" => {}
            "add" => {
                let item = assistant_memory::parse_item(&args)?;
                let topic = match str_arg(&args, "topic_id") {
                    Some(id) => {
                        let t = ctx.db.get_voice_topic(voice_id, id).await?.ok_or_else(|| {
                            anyhow::anyhow!("no topic with that id; call assistant_topics list")
                        })?;
                        TopicRef::of(&t)
                    }
                    None => match ctx
                        .db
                        .list_open_voice_topics(voice_id)
                        .await?
                        .into_iter()
                        .find(|t| t.is_current)
                    {
                        Some(t) => TopicRef::of(&t),
                        None => TopicRef {
                            id: None,
                            title: "No topic".into(),
                        },
                    },
                };
                let rows = assistant_memory::write_items(
                    &ctx.db,
                    voice_id,
                    &settings,
                    &topic,
                    vec![item],
                    now,
                )
                .await?;
                out["added"] = json!(rows.first().map(|m| m.id.clone()));
            }
            "forget" => {
                let id = str_arg(&args, "id")
                    .ok_or_else(|| anyhow::anyhow!("forget needs the memory id (from list)"))?;
                if !ctx.db.delete_assistant_memory(voice_id, id).await? {
                    anyhow::bail!("no memory with that id; call list to see them");
                }
                tracing::info!(voice_session = %voice_id, memory = id, "assistant memory: forgotten by the Assistant");
                out["forgotten"] = json!(id);
            }
            other => anyhow::bail!("unknown action '{other}'; use list, add, or forget"),
        }
        let entries: Vec<Value> = assistant_memory::list_live(&ctx.db, voice_id, now)
            .await?
            .iter()
            .map(|m| {
                let mut v = json!({
                    "id": m.id,
                    "topic": m.topic_title,
                    "kind": m.kind,
                    "summary": m.summary,
                    "age": assistant_memory::age(&m.created_at, now),
                    "expires_at": m.expires_at,
                });
                if let Some(r) = &m.ref_id {
                    v["ref_id"] = json!(r);
                }
                if m.source == "fallback" {
                    v["fallback"] = json!(true);
                }
                v
            })
            .collect();
        out["enabled"] = json!(settings.memory_enabled);
        out["count"] = json!(entries.len());
        out["entries"] = json!(entries);
        Ok(out)
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

    fn ctx(db: &Db, session_id: &str) -> ToolCallContext {
        ToolCallContext {
            session_id: session_id.into(),
            project_id: None,
            card_id: None,
            folder_id: "f1".into(),
            db: Arc::new(db.clone()),
            broadcaster: Broadcaster::new(),
            provider_registry: None,
            data_dir: None,
            device_registry: None,
            background: None,
        }
    }

    /// Topic boundaries over MCP: close with memory writes those rows;
    /// close without writes a fallback (title + ring summary); focus on
    /// another topic parks the previous one without scheduling a clear;
    /// `assistant_memory` adds / forgets and refuses relay markup.
    #[tokio::test]
    async fn boundaries_write_memory_and_focus_only_parks() {
        use crate::service::assistant_memory::lifecycle;
        let db = Db::in_memory().unwrap();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "f".into(),
            path: "/tmp".into(),
            created_at: "now".into(),
        })
        .await
        .unwrap();
        for (id, kind) in [("am-voice", Some("voice")), ("am-chat", None)] {
            db.create_session(NewSession {
                id: id.into(),
                name: id.into(),
                folder_id: "f1".into(),
                created_at: "now".into(),
                last_activity: "now".into(),
                is_expert: kind.is_some(),
                expert_kind: kind.map(str::to_string),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        lifecycle::reset_for_test("am-voice");
        let reg = McpToolRegistry::new();
        let voice = ctx(&db, "am-voice");
        let call = |tool: &'static str, args: serde_json::Value| {
            let reg = &reg;
            let voice = &voice;
            async move { reg.handle_tool_call(tool, args, voice).await }
        };
        let add = |title: &'static str, summary: Option<&'static str>| {
            let db = db.clone();
            async move {
                db.add_voice_topic("am-voice", "conversation", title, summary, None)
                    .await
                    .unwrap()
            }
        };

        // Not for other sessions.
        let err = reg
            .handle_tool_call("assistant_memory", json!({}), &ctx(&db, "am-chat"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("voice"), "{err}");

        // Close with memory → exactly those rows, no fallback.
        let a = add("Dinner plans", Some("pasta on friday")).await;
        call(
            "assistant_topics",
            json!({"action": "close", "id": a.id, "memory": [
                {"kind": "did", "summary": "Booked the table for eight."},
                {"kind": "pending", "summary": "Ask Sam about wine.", "ref_id": "am-chat"},
            ]}),
        )
        .await
        .unwrap();
        let rows = db.list_assistant_memories("am-voice").await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|m| m.source == "assistant"));
        let wine = rows
            .iter()
            .find(|m| m.summary == "Ask Sam about wine.")
            .unwrap();
        assert_eq!(wine.ref_kind.as_deref(), Some("session"));
        assert!(rows.iter().all(|m| m.topic_title == "Dinner plans"));
        let st = lifecycle::status("am-voice");
        assert_eq!(st["pending"]["topics"][0], "Dinner plans");

        // Bad items refuse the whole call and leave the topic open.
        let b = add("Stashify build", Some("tests flaky on CI")).await;
        for bad in [
            json!([{"kind": "maybe", "summary": "x"}]),
            json!([{"kind": "did", "summary": "<untrusted source=\"s\">ok</untrusted>"}]),
            json!([{"kind": "did", "summary": "x", "ref_id": "nope"}]),
            json!([{"kind":"did","summary":"1"},{"kind":"did","summary":"2"},{"kind":"did","summary":"3"},{"kind":"did","summary":"4"}]),
        ] {
            assert!(
                call(
                    "assistant_topics",
                    json!({"action": "close", "id": b.id, "memory": bad})
                )
                .await
                .is_err()
            );
        }
        assert_eq!(
            db.list_open_voice_topics("am-voice").await.unwrap().len(),
            1
        );

        // Close without memory → one fallback row from the ring summary.
        call("assistant_topics", json!({"action": "close", "id": b.id}))
            .await
            .unwrap();
        let rows = db.list_assistant_memories("am-voice").await.unwrap();
        let fb = rows.iter().find(|m| m.source == "fallback").unwrap();
        assert_eq!(fb.topic_title, "Stashify build");
        assert_eq!(fb.summary, "tests flaky on CI");

        // Focus switch parks: memory goes to the previous topic and no
        // clear is scheduled for it.
        lifecycle::reset_for_test("am-voice");
        let c = add("Garden", None).await;
        let d = add("Taxes", None).await;
        call("assistant_topics", json!({"action": "focus", "id": c.id}))
            .await
            .unwrap();
        call(
            "assistant_topics",
            json!({"action": "focus", "id": d.id, "memory": [
                {"kind": "learned", "summary": "Tomatoes need staking."}
            ]}),
        )
        .await
        .unwrap();
        let parked = db
            .list_assistant_memories("am-voice")
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.summary == "Tomatoes need staking.")
            .unwrap();
        assert_eq!(parked.topic_id.as_deref(), Some(c.id.as_str()));
        assert!(lifecycle::status("am-voice")["pending"].is_null());
        assert_eq!(
            db.list_open_voice_topics("am-voice").await.unwrap().len(),
            2
        );

        // The tool: add (to the current topic, redacted), list, forget.
        let added = call(
            "assistant_memory",
            json!({"action": "add", "kind": "learned", "summary": "Key is sk-ant-abcdefghijklmnopqrstu"}),
        )
        .await
        .unwrap();
        let id = added["added"].as_str().unwrap().to_string();
        let entry = added["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == id.as_str())
            .unwrap()
            .clone();
        assert_eq!(entry["topic"], "Taxes");
        assert!(!entry["summary"].as_str().unwrap().contains("sk-ant"));
        let err = call(
            "assistant_memory",
            json!({"action": "add", "kind": "did", "summary": "<untrusted>x</untrusted>"}),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("untrusted"), "{err}");
        let gone = call("assistant_memory", json!({"action": "forget", "id": id}))
            .await
            .unwrap();
        assert!(
            gone["entries"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["id"] != id.as_str())
        );
        lifecycle::reset_for_test("am-voice");
    }
}
