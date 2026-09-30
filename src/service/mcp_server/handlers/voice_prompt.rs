use serde_json::{Value, json};

use super::super::McpToolRegistry;
use crate::service::mcp_server::context::ToolCallContext;
use crate::service::voice_prompt::{self, PromptError, SOURCE_ASSISTANT};
use crate::service::voice_relay::VOICE_EXPERT_KIND;

impl McpToolRegistry {
    /// `voice_prompt` — the voice assistant reads or edits its own system
    /// prompt (`service::voice_prompt`). `get` returns it; `update` replaces
    /// it; `append` adds a line. Changes apply from the next turn.
    ///
    /// `update` / `append` need `confirmed: true`, enforced (and stripped)
    /// before dispatch by `voice_relay::require_voice_confirmation`.
    /// Hard-enforced to voice sessions here as well as in `ToolGate`.
    pub(crate) async fn handle_voice_prompt(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("get");
        let arg = |k: &str| args.get(k).and_then(|v| v.as_str()).map(str::to_string);
        tracing::info!(session_id = %ctx.session_id, action, "MCP tool: voice_prompt");
        let caller = ctx
            .db
            .get_session(&ctx.session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("caller session not found"))?;
        if caller.expert_kind.as_deref() != Some(VOICE_EXPERT_KIND) {
            anyhow::bail!("voice_prompt is only available to the voice assistant session");
        }
        let content = match action {
            "get" => {
                let a = voice_prompt::active(&ctx.db).await?;
                return Ok(json!({
                    "content": a.content,
                    "is_default": a.is_default,
                    "source": a.source,
                    "updated_at": a.updated_at,
                }));
            }
            "update" => arg("content").ok_or_else(|| anyhow::anyhow!("update needs content"))?,
            "append" => {
                let text = arg("text").ok_or_else(|| anyhow::anyhow!("append needs text"))?;
                let current = voice_prompt::active_content(&ctx.db).await?;
                format!("{}\n{}", current.trim_end(), text.trim())
            }
            other => anyhow::bail!("unknown action '{other}'; use get, update or append"),
        };
        let change = match voice_prompt::save(
            &ctx.db,
            &content,
            SOURCE_ASSISTANT,
            arg("note"),
            caller.user_id.clone(),
        )
        .await
        {
            Ok(c) => c,
            Err(PromptError::Invalid(msg)) => anyhow::bail!("{msg}"),
            Err(PromptError::Failed(e)) => return Err(e),
        };
        if !change.changed {
            return Ok(json!({
                "changed": false,
                "message": "That is already your prompt; nothing changed.",
            }));
        }
        voice_prompt::apply_to_voice_session(&ctx.db, ctx.provider_registry.as_deref()).await?;
        let s = change.diff_stats;
        Ok(json!({
            "changed": true,
            "lines_added": s.added,
            "lines_removed": s.removed,
            "message": format!(
                "Saved: {} line(s) added, {} removed. It applies from your next turn.",
                s.added, s.removed
            ),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::db::Db;
    use crate::db::models::{NewFolder, NewSession};
    use crate::service::mcp_server::{McpToolRegistry, ToolCallContext};
    use crate::service::voice_prompt;
    use crate::service::voice_relay::{VOICE_SYSTEM_PROMPT, require_voice_confirmation};

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
    async fn voice_prompt_needs_confirmation_and_voice_session() {
        let db = Db::in_memory().unwrap();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "f".into(),
            path: "/tmp".into(),
            created_at: "now".into(),
        })
        .await
        .unwrap();
        for (id, kind) in [("vp-voice", Some("voice")), ("vp-chat", None)] {
            db.create_session(NewSession {
                id: id.into(),
                name: id.into(),
                folder_id: "f1".into(),
                created_at: "now".into(),
                last_activity: "now".into(),
                is_expert: kind.is_some(),
                expert_kind: kind.map(str::to_string),
                system_prompt: kind.map(|_| VOICE_SYSTEM_PROMPT.to_string()),
                user_id: Some("u1".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        let reg = McpToolRegistry::new();

        // Non-voice session: refused.
        let err = reg
            .handle_tool_call(
                "voice_prompt",
                serde_json::json!({"action": "get"}),
                &ctx(&db, "vp-chat"),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("voice"), "{err}");

        // The dispatch gate: get is free; append without confirmed refused.
        let mut get = serde_json::json!({"action": "get"});
        require_voice_confirmation(&db, "vp-voice", "voice_prompt", &mut get)
            .await
            .unwrap();
        let mut unconfirmed = serde_json::json!({"action": "append", "text": "Be brief."});
        let err = require_voice_confirmation(&db, "vp-voice", "voice_prompt", &mut unconfirmed)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("confirmed"), "{err}");
        let mut confirmed = serde_json::json!({"action": "append", "text": "Be brief.", "note": "asked", "confirmed": true});
        require_voice_confirmation(&db, "vp-voice", "voice_prompt", &mut confirmed)
            .await
            .unwrap();
        assert!(confirmed.get("confirmed").is_none());

        let out = reg
            .handle_tool_call("voice_prompt", confirmed, &ctx(&db, "vp-voice"))
            .await
            .unwrap();
        assert_eq!(out["changed"], true, "{out}");
        assert_eq!(out["lines_added"], 1);
        let active = voice_prompt::active(&db).await.unwrap();
        assert_eq!(active.source, "assistant");
        assert!(active.content.ends_with("\nBe brief."));
        let session = db.get_session("vp-voice").await.unwrap().unwrap();
        assert_eq!(
            session.system_prompt.as_deref(),
            Some(active.content.as_str())
        );
        let history = voice_prompt::history(&db).await.unwrap();
        assert_eq!(history[0].note.as_deref(), Some("asked"));
    }
}
