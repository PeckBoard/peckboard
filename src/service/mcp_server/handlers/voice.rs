use serde_json::Value;

use super::super::McpToolRegistry;
use crate::service::mcp_server::context::ToolCallContext;
use crate::service::voice_relay::VOICE_EXPERT_KIND;

impl McpToolRegistry {
    /// `answer_question` — the voice assistant answers (or dismisses) a
    /// pending question another session asked, on the user's behalf.
    /// Validation happens here; the resolution itself needs the `AppState`,
    /// so the `mcp` route runs `service::questions::resolve_question` off
    /// the `_resolve_question` marker.
    ///
    /// Hard-enforced to voice sessions here as well as in `ToolGate`. Any
    /// target session is fair game — the voice session is global.
    pub(crate) async fn handle_answer_question(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let session_id = args
            .get("session_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("answer_question requires 'session_id'"))?;
        let question_id = args
            .get("question_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("answer_question requires 'question_id'"))?;
        let rejected = args
            .get("rejected")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        tracing::info!(
            session_id = %ctx.session_id,
            target = %session_id,
            question_id,
            rejected,
            "MCP tool: answer_question"
        );

        let caller = ctx
            .db
            .get_session(&ctx.session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("caller session not found"))?;
        if caller.expert_kind.as_deref() != Some(VOICE_EXPERT_KIND) {
            anyhow::bail!("answer_question is only available to the voice assistant session");
        }

        // The voice assistant is global and admin-only (see `routes::voice`):
        // it may answer any session's question, whoever owns it.
        let target = ctx
            .db
            .get_session(session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("session not found: {session_id}"))?;
        // Answer under the voice session owner's authority; a legacy unowned
        // voice row falls back to the target's owner.
        let user_id = caller
            .user_id
            .clone()
            .or_else(|| target.user_id.clone())
            .ok_or_else(|| anyhow::anyhow!("neither session has an owner to answer as"))?;

        let pending =
            crate::service::questions::pending_question_events(&ctx.db, &target.id).await?;
        if !pending.iter().any(|q| q.id == question_id) {
            anyhow::bail!(
                "question {question_id} is not pending on session {session_id} (already answered, dismissed, or unknown)"
            );
        }

        let data = if rejected {
            serde_json::json!({ "question_id": question_id, "rejected": true })
        } else {
            let answers = normalize_answers(args.get("answers")).ok_or_else(|| {
                anyhow::anyhow!(
                    "answer_question requires 'answers' keyed by question index, e.g. {{\"0\": \"...\"}}, or rejected: true"
                )
            })?;
            serde_json::json!({ "question_id": question_id, "answers": answers })
        };

        Ok(serde_json::json!({
            "status": "ok",
            "message": if rejected {
                "Question dismissed; the session resumes."
            } else {
                "Answer delivered; the session resumes."
            },
            "_resolve_question": {
                "session_id": target.id,
                "user_id": user_id,
                "data": data,
            },
        }))
    }
}

/// `resolve_question` reads each answer as a string: keep strings, join a
/// multi-select array, stringify anything else. `None` when there is no
/// non-empty answers object.
fn normalize_answers(answers: Option<&Value>) -> Option<Value> {
    let obj = answers?.as_object()?;
    if obj.is_empty() {
        return None;
    }
    let normalized = obj
        .iter()
        .map(|(k, v)| {
            let s = match v {
                Value::String(s) => s.clone(),
                Value::Array(items) => items
                    .iter()
                    .map(|i| {
                        i.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| i.to_string())
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
                other => other.to_string(),
            };
            (k.clone(), Value::String(s))
        })
        .collect();
    Some(Value::Object(normalized))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::db::Db;
    use crate::db::models::{NewFolder, NewSession};
    use crate::service::mcp_server::{McpToolRegistry, ToolCallContext};

    async fn seed(db: &Db, id: &str, kind: Option<&str>, user: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        db.create_session(NewSession {
            id: id.into(),
            name: id.into(),
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
    async fn answer_question_is_voice_only_and_reaches_any_session() {
        let db = Db::in_memory().unwrap();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "f".into(),
            path: "/tmp".into(),
            created_at: "now".into(),
        })
        .await
        .unwrap();
        seed(&db, "voice", Some("voice"), "u1").await;
        seed(&db, "chat", None, "u1").await;
        seed(&db, "target", None, "u1").await;
        seed(&db, "other", None, "u2").await;
        let q = db
            .append_event(
                "target",
                "question",
                serde_json::json!({"questions": [{"question": "Which DB?"}]}),
            )
            .await
            .unwrap();
        let q2 = db
            .append_event(
                "other",
                "question",
                serde_json::json!({"questions": [{"question": "Secret?"}]}),
            )
            .await
            .unwrap();
        let reg = McpToolRegistry::new();

        // Non-voice caller: refused.
        let err = reg
            .handle_tool_call(
                "answer_question",
                serde_json::json!({"session_id": "target", "question_id": q.id, "answers": {"0": "Postgres"}}),
                &ctx(&db, "chat"),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("voice"), "{err}");
        // Another user's session: the global voice session answers it too,
        // under the voice owner's authority.
        let ok = reg
            .handle_tool_call(
                "answer_question",
                serde_json::json!({"session_id": "other", "question_id": q2.id, "answers": {"0": "x"}}),
                &ctx(&db, "voice"),
            )
            .await
            .unwrap();
        assert_eq!(ok["_resolve_question"]["session_id"], "other");
        assert_eq!(ok["_resolve_question"]["user_id"], "u1");

        // Unknown session: refused.
        let err = reg
            .handle_tool_call(
                "answer_question",
                serde_json::json!({"session_id": "nope", "question_id": q2.id, "answers": {"0": "x"}}),
                &ctx(&db, "voice"),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
        assert!(err.to_string().contains("not found"), "{err}");

        // Happy path: marker carries exactly what resolve_question expects.
        let ok = reg
            .handle_tool_call(
                "answer_question",
                serde_json::json!({"session_id": "target", "question_id": q.id, "answers": {"0": "Postgres"}}),
                &ctx(&db, "voice"),
            )
            .await
            .unwrap();
        assert_eq!(
            ok["_resolve_question"],
            serde_json::json!({
                "session_id": "target",
                "user_id": "u1",
                "data": {"question_id": q.id, "answers": {"0": "Postgres"}},
            })
        );

        // Once resolved, a second answer is refused.
        db.append_event(
            "target",
            "question-resolved",
            serde_json::json!({"question_id": q.id, "answers": {"0": "Postgres"}}),
        )
        .await
        .unwrap();
        let err = reg
            .handle_tool_call(
                "answer_question",
                serde_json::json!({"session_id": "target", "question_id": q.id, "rejected": true}),
                &ctx(&db, "voice"),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not pending"), "{err}");
    }
}
