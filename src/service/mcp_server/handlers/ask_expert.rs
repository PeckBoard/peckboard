//! `ask_expert` — asynchronous Q&A delivery between an ordinary session and
//! a long-lived expert session.
//!
//! LOCKED DESIGN: the exchange is fully asynchronous — the caller never
//! blocks waiting for the expert. The flow has two directions, both built on
//! the existing [`McpToolRegistry::deliver_to_worker`] mechanism (append a
//! `user` event + broadcast `worker-stdin-deliver`):
//!
//! 1. **Ask** (`question` + a target selector): the question is delivered to
//!    the chosen expert session so it sees it on its next turn, and a
//!    context-coupled answer is delivered back to the asking session sourced
//!    from the expert's eagerly-captured `knowledge_summary`. This is the
//!    "write the answer as an event the caller reads on its next turn"
//!    approach — the caller always gets something on its next turn even with
//!    no live agent driving the expert (e.g. in tests / headless).
//! 2. **Reply** (`answer` + `reply_to_session_id`): the genuine async path —
//!    a live expert that has taken a turn on the question delivers its answer
//!    back to the asking session, coupled with which expert / area / the
//!    original question. The expert is told to use this in the ask-mode
//!    prompt.
//!
//! Scope is enforced in both directions against the caller's MCP token: a
//! caller can only reach experts in its own project or globally-scoped
//! experts (`project_id IS NULL`), and an expert can only deliver answers to
//! sessions in its own project (global experts may answer any in-scope
//! caller).

use serde_json::{Value, json};

use super::super::McpToolRegistry;
use crate::db::models::Session;
use crate::service::mcp_server::context::ToolCallContext;

impl McpToolRegistry {
    pub(crate) async fn handle_ask_expert(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        // Reply mode: a (live) expert delivering its answer back to the
        // asking session. Keyed by the presence of both reply fields.
        if let (Some(answer), Some(reply_to)) = (
            args.get("answer").and_then(|v| v.as_str()),
            args.get("reply_to_session_id").and_then(|v| v.as_str()),
        ) {
            let question_echo = args.get("question").and_then(|v| v.as_str());
            return self
                .deliver_expert_answer(ctx, reply_to, answer, question_echo)
                .await;
        }

        // Ask mode.
        let question = args
            .get("question")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("ask_expert requires 'question'"))?;
        let expert_id = args.get("expert_id").and_then(|v| v.as_str());
        let area = args.get("area").and_then(|v| v.as_str());

        tracing::info!(
            session_id = %ctx.session_id,
            expert_id = ?expert_id,
            area = ?area,
            "MCP tool: ask_expert"
        );

        // The caller's token project is the scope boundary. `None` = an
        // unscoped (e.g. global chat) caller, which may only reach globals.
        let caller_project = ctx.project_id.clone();

        // Resolve the target expert, enforcing scope.
        let expert = match expert_id {
            Some(id) => {
                let e = ctx
                    .db
                    .get_expert_session(id)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("expert not found: {id}"))?;
                ensure_expert_in_scope(&e, caller_project.as_deref())?;
                e
            }
            None => {
                let candidates = self
                    .in_scope_experts(ctx, caller_project.as_deref())
                    .await?;
                match pick_expert(candidates, area) {
                    Some(e) => e,
                    None => {
                        return Ok(json!({
                            "status": "no_expert",
                            "message": "no in-scope expert is available to answer; \
                                        spin up experts first or widen the scope",
                        }));
                    }
                }
            }
        };

        let area_label = expert.knowledge_area.clone().unwrap_or_else(|| {
            expert
                .scope_path
                .clone()
                .unwrap_or_else(|| "general".into())
        });

        // 1. Deliver the question to the expert. Coupled with the caller id +
        //    a reply instruction so a live expert can answer via reply-mode.
        let question_msg = format!(
            "[Expert consultation request] (NOT from the user — from another session)\n\
             From session: {caller}\n\
             Your area: {area}\n\n\
             Question: {question}\n\n\
             When you have an answer, deliver it back by calling the `ask_expert` tool with \
             `reply_to_session_id` set to \"{caller}\" and `answer` set to your reply \
             (include the original question for context).",
            caller = ctx.session_id,
            area = area_label,
            question = question,
        );
        self.deliver_to_worker(ctx, &expert.id, &question_msg).await;

        // 2. Deliver a context-coupled answer back to the asking session,
        //    sourced from the expert's eagerly-captured knowledge. The caller
        //    reads this on its next turn (no synchronous wait). A live expert
        //    may additionally reply with a richer answer via reply-mode.
        let knowledge = expert
            .knowledge_summary
            .clone()
            .unwrap_or_else(|| "(this expert has not captured its scope yet)".into());
        let scope = expert.scope_path.clone().unwrap_or_default();
        let answer_msg = format!(
            "[Expert answer — {area} (expert {expert_id})]\n\
             In response to your question: \"{question}\"\n\n\
             {knowledge}\n\n\
             (Source: long-lived knowledge expert covering {scope}. A more specific \
             live answer may arrive on a later turn.)",
            area = area_label,
            expert_id = expert.id,
            question = question,
            knowledge = knowledge,
            scope = if scope.is_empty() {
                "its scope".into()
            } else {
                scope.clone()
            },
        );
        self.deliver_to_worker(ctx, &ctx.session_id, &answer_msg)
            .await;

        Ok(json!({
            "status": "ok",
            "expert_id": expert.id,
            "knowledge_area": expert.knowledge_area,
            "scope_path": expert.scope_path,
            "delivered": true,
            "answer": answer_msg,
            "message": format!(
                "Question delivered to expert {} ({}). An answer was returned and a \
                 live follow-up may arrive on a later turn.",
                expert.id, area_label
            ),
        }))
    }

    /// Reply-mode: deliver an expert's answer back to the asking session,
    /// coupled with which expert / area / the original question.
    async fn deliver_expert_answer(
        &self,
        ctx: &ToolCallContext,
        reply_to: &str,
        answer: &str,
        question_echo: Option<&str>,
    ) -> anyhow::Result<Value> {
        tracing::info!(
            session_id = %ctx.session_id,
            reply_to = %reply_to,
            "MCP tool: ask_expert (reply)"
        );

        let target = ctx
            .db
            .get_session(reply_to)
            .await?
            .ok_or_else(|| anyhow::anyhow!("target session not found: {reply_to}"))?;

        // Scope: a project-scoped expert may only answer sessions in its own
        // project; a global expert (unscoped token) may answer any in-scope
        // caller.
        if let Some(cp) = ctx.project_id.as_deref()
            && target.project_id.as_deref() != Some(cp)
        {
            anyhow::bail!("cannot deliver answer to session in another project: {reply_to}");
        }

        // Identify the replying expert for context.
        let me = ctx.db.get_session(&ctx.session_id).await.ok().flatten();
        let area_label = me
            .as_ref()
            .and_then(|s| s.knowledge_area.clone())
            .unwrap_or_else(|| "expert".into());

        let regarding = match question_echo {
            Some(q) => format!("Regarding your question: \"{q}\"\n\n"),
            None => String::new(),
        };
        let msg = format!(
            "[Expert answer — {area}] (NOT from the user — from an expert session)\n\
             From expert session: {expert}\n\n\
             {regarding}{answer}",
            area = area_label,
            expert = ctx.session_id,
            regarding = regarding,
            answer = answer,
        );
        self.deliver_to_worker(ctx, reply_to, &msg).await;

        Ok(json!({
            "status": "ok",
            "delivered": true,
            "reply_to_session_id": reply_to,
        }))
    }

    /// Experts the caller may consult: those scoped to its project plus
    /// globally-scoped experts. An unscoped caller sees only globals.
    async fn in_scope_experts(
        &self,
        ctx: &ToolCallContext,
        caller_project: Option<&str>,
    ) -> anyhow::Result<Vec<Session>> {
        match caller_project {
            Some(p) => ctx.db.list_expert_sessions_by_scope(p).await,
            None => {
                let all = ctx.db.list_expert_sessions().await?;
                Ok(all.into_iter().filter(|e| e.project_id.is_none()).collect())
            }
        }
    }
}

/// An expert is in scope for a caller if it is global (`project_id IS NULL`)
/// or owned by the caller's own project. Rejects cross-project access.
fn ensure_expert_in_scope(expert: &Session, caller_project: Option<&str>) -> anyhow::Result<()> {
    match expert.project_id.as_deref() {
        None => Ok(()), // global experts are reachable by anyone
        Some(ep) => match caller_project {
            Some(cp) if cp == ep => Ok(()),
            _ => anyhow::bail!(
                "expert {} is out of scope for this caller (belongs to project {})",
                expert.id,
                ep
            ),
        },
    }
}

/// Choose the best expert for an optional `area` hint. Prefers project-scoped
/// experts over globals, and within that, the first whose area / scope / name
/// matches the hint. With no hint (or no match), returns the first knowledge
/// expert, else the first expert.
fn pick_expert(mut candidates: Vec<Session>, area: Option<&str>) -> Option<Session> {
    if candidates.is_empty() {
        return None;
    }
    // Project-scoped experts first, then globals; keep last_activity order
    // (the CRUD already sorts desc) within each group.
    candidates.sort_by_key(|e| e.project_id.is_none());

    if let Some(hint) = area
        .map(|a| a.trim().to_lowercase())
        .filter(|a| !a.is_empty())
    {
        let matched = candidates.iter().find(|e| {
            let in_field = |f: &Option<String>| {
                f.as_deref()
                    .map(|v| v.to_lowercase().contains(&hint))
                    .unwrap_or(false)
            };
            in_field(&e.knowledge_area)
                || in_field(&e.scope_path)
                || e.name.to_lowercase().contains(&hint)
        });
        if let Some(e) = matched {
            return Some(e.clone());
        }
    }

    candidates
        .iter()
        .find(|e| e.expert_kind.as_deref() == Some("knowledge"))
        .or_else(|| candidates.first())
        .cloned()
}
