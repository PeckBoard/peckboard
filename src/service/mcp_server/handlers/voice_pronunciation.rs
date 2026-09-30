use serde_json::{Value, json};

use super::super::McpToolRegistry;
use crate::service::mcp_server::context::ToolCallContext;
use crate::service::tts::lexicon::{self, LexiconError, SOURCE_USER};
use crate::service::voice_relay::VOICE_EXPERT_KIND;

/// How many unknown words `list` reports.
const UNKNOWN_LIMIT: usize = 10;

impl McpToolRegistry {
    /// `voice_pronunciation` — the voice assistant edits the TTS lexicon
    /// (`service::tts::lexicon`) by voice: `add` saves a user pronunciation
    /// (applies to the next sentence), `list` summarizes saved entries plus
    /// the top unknown words, `remove` deletes one.
    ///
    /// Hard-enforced to voice sessions here as well as in `ToolGate`.
    pub(crate) async fn handle_voice_pronunciation(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("list");
        let arg = |k: &str| args.get(k).and_then(|v| v.as_str()).map(str::to_string);
        tracing::info!(session_id = %ctx.session_id, action, "MCP tool: voice_pronunciation");
        let caller = ctx
            .db
            .get_session(&ctx.session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("caller session not found"))?;
        if caller.expert_kind.as_deref() != Some(VOICE_EXPERT_KIND) {
            anyhow::bail!("voice_pronunciation is only available to the voice assistant session");
        }
        let store = lexicon::store(&ctx.db).await?;
        match action {
            "add" => {
                let word = arg("word").ok_or_else(|| anyhow::anyhow!("add needs a word"))?;
                let (resp, ph) = (arg("respelling"), arg("phonemes"));
                let entry = match store.put(&word, None, resp.as_deref(), ph.as_deref()).await {
                    Ok(e) => e,
                    Err(LexiconError::Invalid(msg)) => anyhow::bail!("{msg}"),
                    Err(LexiconError::Failed(e)) => return Err(e),
                };
                Ok(json!({
                    "word": entry.display,
                    "phonemes": entry.phonemes,
                    "message": format!("Saved. \"{}\" is pronounced that way from the next sentence on.", entry.display),
                }))
            }
            "remove" => {
                let word = arg("word").ok_or_else(|| anyhow::anyhow!("remove needs a word"))?;
                let removed = store.delete(&word).await?;
                Ok(json!({
                    "removed": removed,
                    "message": if removed {
                        format!("Removed the pronunciation for \"{word}\".")
                    } else {
                        format!("There was no pronunciation saved for \"{word}\".")
                    },
                }))
            }
            "list" => {
                let entries: Vec<Value> = store
                    .list()
                    .into_iter()
                    .map(|e| {
                        json!({
                            "word": e.display,
                            "respelling": e.respelling,
                            "custom": e.source == SOURCE_USER,
                        })
                    })
                    .collect();
                let unknown: Vec<Value> = store
                    .list_unknown()
                    .await?
                    .into_iter()
                    .take(UNKNOWN_LIMIT)
                    .map(|u| json!({ "word": u.word, "count": u.count }))
                    .collect();
                Ok(json!({
                    "count": entries.len(),
                    "pronunciations": entries,
                    "unknown_words": unknown,
                    "message": "unknown_words are words the voice had no pronunciation for, most spoken first; offer to add the ones the user cares about.",
                }))
            }
            other => anyhow::bail!("unknown action '{other}'; use add, list or remove"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::db::Db;
    use crate::db::models::{NewFolder, NewSession};
    use crate::service::mcp_server::{McpToolRegistry, ToolCallContext};
    use crate::service::tts::lexicon;

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
    async fn voice_pronunciation_add_list_remove() {
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
                user_id: Some("u1".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        let reg = McpToolRegistry::new();
        let call = |sid: &'static str, args: serde_json::Value| {
            let reg = &reg;
            let c = ctx(&db, sid);
            async move { reg.handle_tool_call("voice_pronunciation", args, &c).await }
        };

        let err = call("vp-chat", serde_json::json!({"action": "list"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("voice"), "{err}");

        let added = call(
            "vp-voice",
            serde_json::json!({"action": "add", "word": "Kokoro", "respelling": "KOH-koh-roh"}),
        )
        .await
        .unwrap();
        assert_eq!(added["phonemes"], "kˈOkOɹO");
        let store = lexicon::store(&db).await.unwrap();
        assert_eq!(store.lookup("kokoro", false).unwrap(), "kˈOkOɹO");

        let bad = call(
            "vp-voice",
            serde_json::json!({"action": "add", "word": "Kokoro", "phonemes": "k#"}),
        )
        .await
        .unwrap_err();
        assert!(bad.to_string().contains("'#'"), "{bad}");

        let listed = call("vp-voice", serde_json::json!({"action": "list"}))
            .await
            .unwrap();
        let kokoro = listed["pronunciations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["word"] == "Kokoro")
            .unwrap();
        assert_eq!(kokoro["custom"], true);
        assert_eq!(kokoro["respelling"], "KOH-koh-roh");
        assert!(listed["unknown_words"].is_array());

        let removed = call(
            "vp-voice",
            serde_json::json!({"action": "remove", "word": "kokoro"}),
        )
        .await
        .unwrap();
        assert_eq!(removed["removed"], true);
        assert!(store.lookup("kokoro", false).is_none());
    }
}
