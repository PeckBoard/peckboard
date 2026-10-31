//! `show_view` — the voice assistant drives what page the user is looking
//! at. A voice-only front door to the same resolution and per-user
//! `ui-navigate` frame as the general `ui_open` tool (`ui_control.rs`):
//! the target (project / session / card / folder / view by id or spoken
//! name, or a top-level page) is resolved server-side and only the voice
//! session's owner's browser navigates.

use serde_json::Value;

use super::super::McpToolRegistry;
use crate::service::mcp_server::context::ToolCallContext;
use crate::service::voice_relay::VOICE_EXPERT_KIND;

impl McpToolRegistry {
    /// `show_view` — voice-session-only; see the module docs.
    pub(crate) async fn handle_show_view(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, ?args, "MCP tool: show_view");
        let is_voice = ctx
            .db
            .get_session(&ctx.session_id)
            .await?
            .is_some_and(|s| s.expert_kind.as_deref() == Some(VOICE_EXPERT_KIND));
        if !is_voice {
            anyhow::bail!("show_view is only available to the voice assistant session");
        }
        if !crate::service::assistant_settings::load(&ctx.db)
            .await
            .auto_show_on_topic
            && turn_started_by_topic_focus(ctx).await?
        {
            anyhow::bail!(
                "show_view is off for topic changes: the user only switched the topic ring, so leave their screen where it is. Open something only when they ask to see it."
            );
        }
        let args = topic_id_to_target(ctx, args).await?;
        let caller = self.ui_caller(ctx).await?;
        self.open_target(ctx, &caller, &args, "show_view").await
    }
}

/// True when the voice session's latest user message is the "Let's talk
/// about <current topic>." that a topic focus sends (a click on the ring, or
/// the user saying exactly that): a pure topic change, not a request to see
/// anything. Topic focus and the screen are separate concerns, so with
/// `auto_show_on_topic` off such a turn may not navigate.
async fn turn_started_by_topic_focus(ctx: &ToolCallContext) -> anyhow::Result<bool> {
    let topics = ctx.db.list_open_voice_topics(&ctx.session_id).await?;
    let Some(current) = topics.into_iter().find(|t| t.is_current) else {
        return Ok(false);
    };
    let Some(last) = ctx
        .db
        .latest_event_of_kinds(&ctx.session_id, &["user"])
        .await?
    else {
        return Ok(false);
    };
    let msg = crate::service::voice_topics::focus_message(&current.title);
    // Exactly that message, so "let's talk about X and show me it" still may.
    let data: Value = serde_json::from_str(&last.data).unwrap_or(Value::Null);
    Ok(data.as_object().is_some_and(|o| {
        o.values()
            .any(|v| v.as_str().map(str::trim) == Some(msg.as_str()))
    }))
}
/// The Assistant often has a topic-ring id at hand (`assistant_topics`
/// returns them) and passes it to `show_view` as if it were the session or
/// project it stands for. Swap an open topic's id for its target, and refuse
/// a conversation topic (it has no page) with a message the model can act on.
async fn topic_id_to_target(ctx: &ToolCallContext, mut args: Value) -> anyhow::Result<Value> {
    let Some(id) = args.get("id").and_then(Value::as_str).map(str::to_string) else {
        return Ok(args);
    };
    let topics = ctx.db.list_open_voice_topics(&ctx.session_id).await?;
    let Some(topic) = topics.into_iter().find(|t| t.id == id) else {
        return Ok(args);
    };
    match (topic.kind.as_str(), topic.ref_id) {
        ("session" | "project", Some(ref_id)) => {
            args["id"] = Value::String(ref_id);
            args["target"] = Value::String(topic.kind.clone());
            Ok(args)
        }
        _ => anyhow::bail!(
            "'{id}' is the conversation topic \"{}\", not a page; pass a session, project, card, or folder id or name instead",
            topic.title
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::db::Db;
    use crate::db::models::{NewFolder, NewProject, NewSession};
    use crate::service::mcp_server::{McpToolRegistry, ToolCallContext};
    use crate::ws::broadcaster::Broadcaster;

    async fn seed_session(db: &Db, id: &str, name: &str, kind: Option<&str>, user: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        db.create_session(NewSession {
            id: id.into(),
            name: name.into(),
            folder_id: "f1".into(),
            created_at: now.clone(),
            last_activity: now,
            is_expert: kind.is_some(),
            expert_kind: kind.map(str::to_string),
            user_id: Some(user.into()),
            ..Default::default()
        })
        .await
        .unwrap();
    }

    async fn seed_project(db: &Db, id: &str, name: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        db.create_project(NewProject {
            id: id.into(),
            name: name.into(),
            context: String::new(),
            folder_id: "f1".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "default".into(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: false,
            worker_communication: false,
            created_at: now.clone(),
            worktree_isolation: false,
            last_accessed_at: now,
            budget_usd_cents: None,
            budget_period: None,
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn show_view_resolves_names_and_navigates_the_owner() {
        let db = Db::in_memory().unwrap();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "infra".into(),
            path: "/tmp".into(),
            created_at: "now".into(),
        })
        .await
        .unwrap();
        seed_session(&db, "voice", "Voice assistant", Some("voice"), "u1").await;
        seed_session(&db, "chat", "chat", None, "u1").await;
        seed_session(&db, "s-dev", "Stashify dev", None, "u1").await;
        seed_session(&db, "s-other", "Stashify secrets", None, "u2").await;
        seed_project(&db, "p1", "Stashify").await;
        seed_project(&db, "p2", "Web app").await;
        seed_project(&db, "p3", "Web api").await;

        let broadcaster = Broadcaster::new();
        let mut rx = broadcaster.subscribe_user_frames();
        broadcaster.register_connection(1, "u1");
        let ctx = |sid: &str| ToolCallContext {
            session_id: sid.into(),
            project_id: None,
            card_id: None,
            folder_id: "f1".into(),
            db: Arc::new(db.clone()),
            broadcaster: broadcaster.clone(),
            provider_registry: None,
            data_dir: None,
            device_registry: None,
            background: None,
        };
        let reg = McpToolRegistry::new();
        let call = |args: serde_json::Value, sid: &'static str| {
            let reg = &reg;
            let c = ctx(sid);
            async move { reg.handle_tool_call("show_view", args, &c).await }
        };

        // Voice-only.
        let err = call(serde_json::json!({"name": "stashify"}), "chat")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("voice"), "{err}");

        // Exact project name beats the "Stashify dev" session prefix match.
        let ok = call(serde_json::json!({"name": "stashify"}), "voice")
            .await
            .unwrap();
        assert_eq!(ok["opened"]["target"], "project");
        assert_eq!(ok["opened"]["id"], "p1");
        let f = rx.try_recv().unwrap();
        assert!(f.is_for(1), "the voice user's browser");
        assert_eq!(f.event().event_type, "ui-navigate");
        assert_eq!(f.event().session_id, "voice");
        assert_eq!(f.event().data["id"], "p1");

        // Session target: fuzzy, and another user's session never matches.
        let ok = call(
            serde_json::json!({"target": "session", "name": "stashify"}),
            "voice",
        )
        .await
        .unwrap();
        assert_eq!(ok["opened"]["id"], "s-dev");

        // Folder by name.
        let ok = call(serde_json::json!({"name": "infra"}), "voice")
            .await
            .unwrap();
        assert_eq!(ok["opened"]["target"], "folder");
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        // Ambiguous: candidates, no navigation.
        let amb = call(
            serde_json::json!({"target": "project", "name": "web"}),
            "voice",
        )
        .await
        .unwrap();
        assert_eq!(amb["status"], "ambiguous");
        assert_eq!(amb["candidates"].as_array().unwrap().len(), 2);
        assert!(rx.try_recv().is_err());

        // Page.
        let ok = call(
            serde_json::json!({"target": "page", "name": "Settings"}),
            "voice",
        )
        .await
        .unwrap();
        assert_eq!(ok["opened"]["page"], "settings");

        // A topic-ring id stands in for its session; a conversation topic
        // has no page and says so.
        let t = db
            .add_voice_topic("voice", "session", "Stashify dev", None, Some("s-dev"))
            .await
            .unwrap();
        let ok = call(
            serde_json::json!({"target": "session", "id": t.id}),
            "voice",
        )
        .await
        .unwrap();
        assert_eq!(ok["opened"]["id"], "s-dev");
        let c = db
            .add_voice_topic("voice", "conversation", "Anime Cap", None, None)
            .await
            .unwrap();
        let err = call(
            serde_json::json!({"target": "session", "id": c.id}),
            "voice",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("conversation topic"), "{err}");
    }

    /// Topic focus and the screen are separate: with `auto_show_on_topic`
    /// off (the default) a turn started by "Let's talk about <topic>." may
    /// not navigate, while an explicit request still does; turning the
    /// setting on restores navigation on focus.
    #[tokio::test]
    async fn topic_focus_alone_does_not_navigate_but_show_view_does() {
        use crate::db::models::NewEvent;
        use crate::service::assistant_settings::{self, AssistantSettings};

        let db = Db::in_memory().unwrap();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "infra".into(),
            path: "/tmp".into(),
            created_at: "now".into(),
        })
        .await
        .unwrap();
        seed_session(&db, "voice", "Voice assistant", Some("voice"), "u1").await;
        seed_session(&db, "s-dev", "Stashify dev", None, "u1").await;
        let topic = db
            .add_voice_topic("voice", "session", "Stashify dev", None, Some("s-dev"))
            .await
            .unwrap();
        db.focus_voice_topic("voice", &topic.id).await.unwrap();
        let mut seq = 0;
        let mut say = |text: String| {
            seq += 1;
            let db = db.clone();
            let s = seq;
            async move {
                db.create_event(NewEvent {
                    id: format!("e{s}"),
                    session_id: "voice".into(),
                    seq: s,
                    ts: s as i64,
                    kind: "user".into(),
                    data: serde_json::json!({ "text": text }).to_string(),
                })
                .await
                .unwrap();
            }
        };

        let broadcaster = Broadcaster::new();
        let mut rx = broadcaster.subscribe_user_frames();
        broadcaster.register_connection(1, "u1");
        let ctx = ToolCallContext {
            session_id: "voice".into(),
            project_id: None,
            card_id: None,
            folder_id: "f1".into(),
            db: Arc::new(db.clone()),
            broadcaster: broadcaster.clone(),
            provider_registry: None,
            data_dir: None,
            device_registry: None,
            background: None,
        };
        let reg = McpToolRegistry::new();
        let show = serde_json::json!({"target": "session", "name": "stashify dev"});

        // Default (off): the focus turn may not navigate, and nothing is sent.
        say(crate::service::voice_topics::focus_message("Stashify dev")).await;
        let err = reg
            .handle_tool_call("show_view", show.clone(), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("topic changes"), "{err}");
        assert!(rx.try_recv().is_err(), "focus alone must not navigate");

        // An explicit request navigates.
        say("show me the stashify dev session".into()).await;
        let ok = reg
            .handle_tool_call("show_view", show.clone(), &ctx)
            .await
            .unwrap();
        assert_eq!(ok["opened"]["id"], "s-dev");
        assert_eq!(rx.try_recv().unwrap().event().event_type, "ui-navigate");

        // Setting on: a focus turn may navigate again.
        assistant_settings::save(
            &db,
            &AssistantSettings {
                auto_show_on_topic: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        say(crate::service::voice_topics::focus_message("Stashify dev")).await;
        reg.handle_tool_call("show_view", show, &ctx).await.unwrap();
        assert_eq!(rx.try_recv().unwrap().event().event_type, "ui-navigate");
    }
}
