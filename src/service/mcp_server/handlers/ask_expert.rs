//! `ask_expert` — asynchronous Q&A delivery between an ordinary session and
//! a long-lived expert session.
//!
//! LOCKED DESIGN: the exchange is fully asynchronous — the caller never
//! blocks waiting for the expert. Every delivery lands the same way a user
//! message does (see [`deliver_as_user_message`]): a `user` event is persisted
//! + broadcast, and — when a live app is present — the target session is
//! resumed via `ExpertDispatcher::resume_session` (`send_or_queue`), so an idle
//! expert actually spawns a turn to answer rather than only seeing the question
//! on some future manual run. In headless / test contexts (no dispatcher) the
//! event is still persisted; only the resume is skipped. The flow has two
//! directions:
//!
//! 1. **Ask** (`question` + a target selector): the question is delivered to
//!    the chosen expert session (resuming it). Nothing is delivered back to the
//!    asking session synchronously — the expert's pre-captured
//!    `knowledge_summary` is not an answer to this specific question, so it is
//!    no longer echoed back. The caller receives only the genuine answer the
//!    live expert produces via reply-mode, which arrives as a later event.
//! 2. **Reply** (`answer` + `reply_to_session_id`): the genuine async path —
//!    a live expert that has taken a turn on the question delivers its answer
//!    back to the asking session, coupled with which expert / area / the
//!    original question. The expert is told to use this in the ask-mode
//!    prompt.
//!
//! Scope is enforced in both directions against the caller's MCP token, with
//! a deliberate carve-out for **knowledge** experts (which only ever answer
//! within their own codebase `scope_path` boundary):
//!
//! - Ask: a caller may reach (a) globally-scoped experts (`project_id IS
//!   NULL`) and (b) experts in its own project. Additionally a **chat
//!   session** (an unscoped token) may reach any **knowledge** expert
//!   regardless of project, to consult it about stuff inside that expert's
//!   boundary. Workers (a project-scoped token) stay confined to their own
//!   project plus globals. **Question** experts stay project/global-scoped:
//!   they hold accumulated, potentially private user Q&A and have no codebase
//!   boundary, so they are never reachable cross-project.
//! - Reply: a **knowledge** expert may deliver its answer back to a
//!   cross-project **chat session** that consulted it (never a worker in
//!   another project). A project-scoped **question** expert may only answer
//!   sessions in its own project (global experts may answer any in-scope
//!   caller). **PM** experts are project-scoped like question experts, so
//!   the same rule confines their replies to their own project.
//!
//! The per-project **PM expert** is additionally addressable by the
//! shorthand "pm" (as `expert_id` or `area`) or by its stable
//! `pm-expert-project-<id>`, resolving to the caller's own project's PM
//! expert and lazily ensuring the row exists (idempotent upsert) — so it is
//! reachable even on DBs whose bootstrap predates the PM-expert feature.

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

        // Resolve the target expert, enforcing scope. The PM expert is
        // special-cased: the "pm" shorthand (or its stable id) resolves to
        // the caller's own project's PM expert, lazily ensured.
        let expert = if is_pm_target(expert_id, area, caller_project.as_deref()) {
            self.resolve_pm_expert(ctx, caller_project.as_deref())
                .await?
        } else {
            match expert_id {
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
        self.deliver_as_user_message(ctx, &expert.id, &question_msg, "expert-consultation")
            .await;

        // No synchronous placeholder is delivered back to the caller: the
        // expert's pre-captured knowledge_summary is not an answer to this
        // specific question. The caller receives only the genuine answer the
        // live expert produces via reply-mode (coupled with the original
        // question), which arrives as a later event. The asked question is
        // echoed in the tool result for the caller's own record.

        Ok(json!({
            "status": "ok",
            "expert_id": expert.id,
            "knowledge_area": expert.knowledge_area,
            "scope_path": expert.scope_path,
            "question": question,
            "delivered": true,
            "message": format!(
                "Question delivered to expert {} ({}). The expert's answer will \
                 arrive as a later event once it takes a turn.",
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

        // Identify the replying expert for context.
        let me = ctx.db.get_session(&ctx.session_id).await.ok().flatten();

        // Scope: a project-scoped *question* expert may only answer sessions
        // in its own project; a global expert (unscoped token) may answer any
        // in-scope caller. A *knowledge* expert may answer cross-project, but
        // only a **chat session** (the non-worker caller that's allowed to
        // consult it cross-project in the first place) — never a worker in
        // another project.
        let me_is_knowledge = me.as_ref().map(is_knowledge_expert).unwrap_or(false);
        let cross_project_ok = me_is_knowledge && !target.is_worker;
        if !cross_project_ok
            && let Some(cp) = ctx.project_id.as_deref()
            && target.project_id.as_deref() != Some(cp)
        {
            anyhow::bail!("cannot deliver answer to session in another project: {reply_to}");
        }

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
        self.deliver_as_user_message(ctx, reply_to, &msg, "expert-answer")
            .await;

        Ok(json!({
            "status": "ok",
            "delivered": true,
            "reply_to_session_id": reply_to,
        }))
    }

    /// Resolve the caller's per-project PM expert by its stable
    /// `pm-expert-project-<id>`, lazily ensuring the row exists (idempotent
    /// upsert) so the PM expert is reachable even when no bootstrap has run
    /// for this project yet. In scope by construction: it always belongs to
    /// the caller's own project.
    pub(super) async fn resolve_pm_expert(
        &self,
        ctx: &ToolCallContext,
        caller_project: Option<&str>,
    ) -> anyhow::Result<Session> {
        let project_id = caller_project.ok_or_else(|| {
            anyhow::anyhow!("the PM expert is per-project; this caller has no project scope")
        })?;
        let id = crate::service::pm_expert::project_pm_expert_id(project_id);
        if let Some(e) = ctx.db.get_expert_session(&id).await? {
            return Ok(e);
        }
        let project = ctx
            .db
            .get_project(project_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("project not found: {project_id}"))?;
        crate::service::pm_expert::ensure_project_pm_expert(&ctx.db, &project).await
    }

    /// Experts the caller may consult: those scoped to its project, plus
    /// globally-scoped experts, plus any knowledge expert in another project
    /// (which only answers within its own boundary). An unscoped caller sees
    /// globals plus every knowledge expert.
    async fn in_scope_experts(
        &self,
        ctx: &ToolCallContext,
        caller_project: Option<&str>,
    ) -> anyhow::Result<Vec<Session>> {
        let all = ctx.db.list_expert_sessions().await?;
        Ok(all
            .into_iter()
            .filter(|e| expert_in_scope(e, caller_project))
            .collect())
    }

    /// Deliver `message` to `target_session_id` the same way a user message
    /// arrives: persist a `user` event (tagged `source`) + broadcast it, then —
    /// when a live dispatcher is present — resume the target via
    /// `send_or_queue` so an idle session spawns a turn (and a running one gets
    /// the message queued / injected mid-stream). With no dispatcher (headless
    /// / tests) the event is still persisted; only the resume is skipped.
    pub(super) async fn deliver_as_user_message(
        &self,
        ctx: &ToolCallContext,
        target_session_id: &str,
        message: &str,
        source: &str,
    ) {
        if let Err(e) = crate::service::delivery::persist_user_message(
            &ctx.db,
            &ctx.broadcaster,
            target_session_id,
            message,
            source,
        )
        .await
        {
            tracing::warn!(
                session_id = %target_session_id,
                "ask_expert: failed to persist delivery: {e}"
            );
            return;
        }

        if let Some(dispatcher) = &ctx.expert_dispatcher
            && let Err(e) = dispatcher.resume_session(target_session_id, message).await
        {
            tracing::warn!(
                session_id = %target_session_id,
                "ask_expert: failed to resume session after delivery: {e}"
            );
        }
    }
}

/// True when the caller addressed the per-project PM expert: the shorthand
/// "pm" (as `expert_id` or, with no `expert_id`, as the `area` hint), or the
/// caller's own project's stable `pm-expert-project-<id>`. Another project's
/// PM expert deliberately does NOT match — it falls through to the generic
/// path, where the scope check rejects it.
fn is_pm_target(expert_id: Option<&str>, area: Option<&str>, caller_project: Option<&str>) -> bool {
    let is_pm = |s: &str| s.trim().eq_ignore_ascii_case("pm");
    match expert_id {
        Some(id) => {
            is_pm(id)
                || caller_project
                    .is_some_and(|p| id == crate::service::pm_expert::project_pm_expert_id(p))
        }
        None => area.is_some_and(is_pm),
    }
}

/// A knowledge expert answers only within its codebase `scope_path` boundary,
/// so it is safe to consult cross-project. A question expert holds accumulated
/// user Q&A with no boundary and stays project/global-scoped.
fn is_knowledge_expert(expert: &Session) -> bool {
    expert.expert_kind.as_deref() == Some("knowledge")
}

/// An expert is in scope for a caller if it is global (`project_id IS NULL`)
/// or owned by the caller's own project. Additionally, a **chat session** (an
/// unscoped token — `caller_project` is `None`) may reach any **knowledge**
/// expert, since it only answers within its own boundary. Workers (a
/// project-scoped token) stay confined to their own project plus globals, and
/// cross-project access to a *question* expert is always rejected.
fn expert_in_scope(expert: &Session, caller_project: Option<&str>) -> bool {
    if expert.project_id.is_none() {
        return true;
    }
    if caller_project.is_some() && expert.project_id.as_deref() == caller_project {
        return true;
    }
    caller_project.is_none() && is_knowledge_expert(expert)
}

fn ensure_expert_in_scope(expert: &Session, caller_project: Option<&str>) -> anyhow::Result<()> {
    if expert_in_scope(expert, caller_project) {
        Ok(())
    } else {
        anyhow::bail!(
            "expert {} is out of scope for this caller (question-expert for project {})",
            expert.id,
            expert.project_id.as_deref().unwrap_or("?"),
        )
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
