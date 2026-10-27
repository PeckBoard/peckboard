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
        let caller = self.ui_caller(ctx).await?;
        self.open_target(ctx, &caller, &args, "show_view").await
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
    }
}
