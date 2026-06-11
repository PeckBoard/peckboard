//! `pm_record_decision` / `pm_check_decisions` / `pm_escalate_to_user` — the
//! MCP surface of the per-project PM decision log (see
//! `crate::db::crud::pm_decisions`).
//!
//! `pm_record_decision` is ADD-only for ordinary callers: superseding an
//! existing decision requires the PM expert acting under an outstanding
//! user authorization, granted one-shot by
//! [`crate::service::pm_expert::deliver_pm_user_answer`] when the user
//! answers an escalated question — decisions change only by express user
//! decision. After a non-PM session records a decision, the PM expert is
//! notified asynchronously (same delivery pattern as ask_expert) so its
//! context stays current.
//!
//! `pm_check_decisions` is deliberately SYNCHRONOUS: it reads the active
//! (non-superseded) decision set straight from the DB and returns it, so a
//! worker can self-check a planned change without blocking on any expert.
//!
//! Both tools resolve their project from the MCP token's scope first; an
//! explicit `project_id` input is honoured only when the calling session has
//! no project context (e.g. a plain chat session), so chat sessions can
//! consult and append to a project's decision log too.
//!
//! `pm_escalate_to_user` is callable ONLY by the project's PM expert: it
//! parks a question the PM cannot answer from recorded decisions as a
//! pending row (`pending_count > 0` is the waiting-for-user state the
//! frontend consumes) until the user answers.

use serde_json::{Value, json};

use super::super::McpToolRegistry;
use crate::db::models::PmDecision;
use crate::service::mcp_server::context::{ScopedProjectId, ToolCallContext};

fn required_str<'a>(args: &'a Value, field: &str, tool: &str) -> anyhow::Result<&'a str> {
    args.get(field)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{tool} requires a non-empty '{field}'"))
}

/// Resolve the project for `pm_record_decision` / `pm_check_decisions`.
/// The session's project scope (from the MCP token) is authoritative —
/// `scope_project` rejects an explicit `project_id` that conflicts with it.
/// The explicit input is honoured only as a fallback for sessions with no
/// project context (e.g. plain chat sessions), and must name an existing
/// project. `pm_escalate_to_user` deliberately does NOT use this: it stays
/// token-scope-only.
async fn resolve_pm_project(
    args: &Value,
    ctx: &ToolCallContext,
) -> anyhow::Result<ScopedProjectId> {
    let explicit = args
        .get("project_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if ctx.project_id.is_none()
        && let Some(p) = explicit
    {
        ctx.db
            .get_project(p)
            .await?
            .ok_or_else(|| anyhow::anyhow!("project not found: {p}"))?;
    }
    ctx.scope_project(explicit).await
}

/// The shape both tools return per decision: `title` is the stored question,
/// `decision` the stored answer, `decided_at` the answer timestamp.
fn decision_json(d: &PmDecision) -> Value {
    json!({
        "id": d.id,
        "title": d.question,
        "decision": d.answer,
        "decided_at": d.answered_at,
    })
}

impl McpToolRegistry {
    pub(crate) async fn handle_pm_record_decision(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let title = required_str(&args, "title", "pm_record_decision")?;
        let decision = required_str(&args, "decision", "pm_record_decision")?;
        let rationale = args
            .get("rationale")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let supersedes = args
            .get("supersedes_decision_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());

        // Token scope is authoritative; explicit project_id is only a
        // fallback for sessions with no project context.
        let project = resolve_pm_project(&args, ctx).await?;
        let pm_expert_id = crate::service::pm_expert::project_pm_expert_id(project.as_str());
        let caller_is_pm_expert = ctx.session_id == pm_expert_id;

        let answer = match rationale {
            Some(r) => format!("{decision}\n\nRationale: {r}"),
            None => decision.to_string(),
        };

        if let Some(old_id) = supersedes {
            if !caller_is_pm_expert {
                anyhow::bail!(
                    "cannot supersede decision {old_id}: changing an existing decision \
                     requires the PM expert acting with explicit user authorization. \
                     Workers may only ADD new decisions — route this change through the \
                     PM expert (`ask_expert` with area \"pm\") so the user can authorize it."
                );
            }
            if !ctx.pm_authorizations.consume(project.as_str()) {
                anyhow::bail!(
                    "cannot supersede decision {old_id}: no outstanding user authorization. \
                     Decisions change only by express user decision — escalate via \
                     `pm_escalate_to_user` and wait for the user's answer."
                );
            }
            // The grant is consumed; restore it if the mutation fails so a
            // transient error doesn't burn the user's authorization.
            let superseding = match self
                .supersede_in_project(ctx, project.as_str(), old_id, title, &answer)
                .await
            {
                Ok(d) => d,
                Err(e) => {
                    ctx.pm_authorizations.grant(project.as_str());
                    return Err(e);
                }
            };

            tracing::info!(
                session_id = %ctx.session_id,
                project_id = %project.as_str(),
                old_id = %old_id,
                new_id = %superseding.id,
                "MCP tool: pm_record_decision (user-authorized supersession)"
            );
            self.export_pm_decisions_if_possible(ctx, project.as_str())
                .await;

            return Ok(json!({
                "status": "ok",
                "decision": decision_json(&superseding),
                "superseded_decision_id": old_id,
                "message": "Decision superseded under express user authorization.",
            }));
        }

        tracing::info!(
            session_id = %ctx.session_id,
            project_id = %project.as_str(),
            title = %title,
            "MCP tool: pm_record_decision"
        );

        let recorded = ctx
            .db
            .record_decision(project.as_str(), title, &answer, Some(&ctx.session_id))
            .await?;

        self.export_pm_decisions_if_possible(ctx, project.as_str())
            .await;

        // Keep the PM expert current: notify it of decisions recorded by any
        // other session. Best-effort — a failed notification must not undo or
        // fail the already-persisted decision.
        if !caller_is_pm_expert {
            let rationale_line = rationale
                .map(|r| format!("Rationale: {r}\n"))
                .unwrap_or_default();
            let note = format!(
                "[PM decision recorded] (NOT from the user — automated notification)\n\
                 Session {caller} recorded a new decision in this project's decision log:\n\n\
                 Title: {title}\n\
                 Decision: {decision}\n\
                 {rationale_line}\
                 Decision id: {id}\n\n\
                 No reply is required. Review it against the decisions you already hold \
                 and raise any conflict with the user.",
                caller = ctx.session_id,
                id = recorded.id,
            );
            match self.resolve_pm_expert(ctx, Some(project.as_str())).await {
                Ok(pm) => {
                    self.deliver_as_user_message(ctx, &pm.id, &note, "pm-decision-recorded")
                        .await
                }
                Err(e) => tracing::warn!(
                    project_id = %project.as_str(),
                    "pm_record_decision: could not notify PM expert: {e}"
                ),
            }
        }

        Ok(json!({
            "status": "ok",
            "decision": decision_json(&recorded),
            "message": "Decision recorded in the project's PM decision log.",
        }))
    }

    pub(crate) async fn handle_pm_check_decisions(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let planned_change = required_str(&args, "planned_change", "pm_check_decisions")?;
        let keywords: Vec<String> = args
            .get("topic_keywords")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.trim().to_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        // Token scope is authoritative; explicit project_id is only a
        // fallback for sessions with no project context.
        let project = resolve_pm_project(&args, ctx).await?;

        tracing::info!(
            session_id = %ctx.session_id,
            project_id = %project.as_str(),
            planned_change = %planned_change,
            "MCP tool: pm_check_decisions"
        );

        let active = ctx.db.list_answered_pm_decisions(project.as_str()).await?;

        // The keyword filter narrows but never empties: when nothing matches,
        // the full active set is returned so a bad keyword can't hide a
        // relevant decision from the caller.
        let matches_keywords = |d: &PmDecision| {
            let hay = format!(
                "{}\n{}",
                d.question,
                d.answer.as_deref().unwrap_or_default()
            )
            .to_lowercase();
            keywords.iter().any(|k| hay.contains(k.as_str()))
        };
        let filtered: Vec<&PmDecision> = if keywords.is_empty() {
            active.iter().collect()
        } else {
            let narrowed: Vec<&PmDecision> =
                active.iter().filter(|d| matches_keywords(d)).collect();
            if narrowed.is_empty() {
                active.iter().collect()
            } else {
                narrowed
            }
        };

        let decisions: Vec<Value> = filtered.iter().map(|d| decision_json(d)).collect();

        Ok(json!({
            "status": "ok",
            "count": decisions.len(),
            "decisions": decisions,
            "instruction": "Check your planned change against every decision above. \
                            If any returned decision is ambiguous w.r.t. your planned \
                            change, ask the PM expert via ask_expert before proceeding.",
        }))
    }

    pub(crate) async fn handle_pm_escalate_to_user(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let question = required_str(&args, "question", "pm_escalate_to_user")?;
        let context = args
            .get("context")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let asking_session_id = args
            .get("asking_session_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());

        // Project scope comes from the MCP token only — never from input.
        let project = ctx.scope_project(None).await?;

        // Callable ONLY by this project's PM expert: the caller must be the
        // project's stable PM-expert session AND its row must carry
        // expert_kind "pm".
        let pm_expert_id = crate::service::pm_expert::project_pm_expert_id(project.as_str());
        let caller_kind = ctx
            .db
            .get_session(&ctx.session_id)
            .await?
            .and_then(|s| s.expert_kind);
        if ctx.session_id != pm_expert_id || caller_kind.as_deref() != Some("pm") {
            anyhow::bail!(
                "pm_escalate_to_user is reserved for this project's PM expert. Route the \
                 question through the PM expert instead: call `ask_expert` with expert_id \
                 (or area) \"pm\" — it escalates to the user when no recorded decision covers \
                 the matter."
            );
        }

        // Provenance must stay inside the caller's project.
        if let Some(asker) = asking_session_id {
            ctx.scope_session(asker).await?;
        }

        tracing::info!(
            session_id = %ctx.session_id,
            project_id = %project.as_str(),
            question = %question,
            "MCP tool: pm_escalate_to_user"
        );

        let stored_question = match context {
            Some(c) => format!("{question}\n\nContext: {c}"),
            None => question.to_string(),
        };
        let pending = ctx
            .db
            .create_pending_question(project.as_str(), &stored_question, asking_session_id)
            .await?;

        self.export_pm_decisions_if_possible(ctx, project.as_str())
            .await;

        let pending_count = ctx.db.pending_pm_decision_count(project.as_str()).await?;

        Ok(json!({
            "status": "ok",
            "pending_question_id": pending.id,
            "pending_count": pending_count,
            "message": "Escalated: the question is now pending for the user. Their answer \
                        will be delivered to you as an express user decision; only then may \
                        an existing decision be superseded.",
        }))
    }

    /// Supersede `old_id` with a new answered decision, refusing to reach
    /// across projects (the CRUD layer takes a bare row id).
    async fn supersede_in_project(
        &self,
        ctx: &ToolCallContext,
        project_id: &str,
        old_id: &str,
        title: &str,
        answer: &str,
    ) -> anyhow::Result<PmDecision> {
        let old = ctx
            .db
            .get_pm_decision(old_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("pm decision not found: {old_id}"))?;
        if old.project_id != project_id {
            anyhow::bail!("pm decision {old_id} belongs to another project");
        }
        ctx.db.supersede_decision(old_id, title, answer).await
    }

    /// Re-export the durable decision file after a mutation. Best-effort: a
    /// disk hiccup must not undo the already-persisted DB change, and
    /// contexts without a data dir (tests / headless) rely on the boot-time
    /// backfill instead.
    async fn export_pm_decisions_if_possible(&self, ctx: &ToolCallContext, project_id: &str) {
        let Some(data_dir) = &ctx.data_dir else {
            return;
        };
        if let Err(e) =
            crate::service::pm_expert::export_pm_decisions(&ctx.db, data_dir, project_id).await
        {
            tracing::warn!(project_id = %project_id, "failed to re-export PM decisions: {e}");
        }
    }
}
