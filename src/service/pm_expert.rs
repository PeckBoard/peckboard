//! The "PM expert" — a per-project long-lived expert that is the durable
//! store of project-direction and business-logic decisions. Workers consult
//! it to learn (or check against) decisions the user has made; unknown
//! matters escalate to the user (escalation tooling lands separately — this
//! module owns only the lifecycle and the role prompt).
//!
//! LOCKED DESIGN (mirrors [`crate::service::question_expert`]):
//! - PM experts have `expert_kind = "pm"`, `is_permanent = true`, and a
//!   STABLE/deterministic session id derived from `project_id`, so they
//!   survive restarts and rehydrate under the same id.
//! - Creation is idempotent: re-running `ensure_project_pm_expert` never
//!   clobbers an existing row (see [`Db::upsert_permanent_expert`]), so the
//!   accumulated decision history survives restarts and repeated ensures.
//! - Exactly one per project; there is no global PM expert.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::db::Db;
use crate::db::models::{NewSession, PmDecision, Project, Session};
use crate::service::mcp_server::ExpertDispatcher;
use crate::ws::broadcaster::{Broadcaster, WsEvent};

/// App-wide store of outstanding user authorizations to change recorded PM
/// decisions. Decisions change ONLY by express user decision: a grant is
/// issued per project when [`deliver_pm_user_answer`] feeds a user's answer
/// back into the PM session, and consumed (one-shot) by the PM expert's
/// follow-up `pm_record_decision` call that sets `supersedes_decision_id`.
///
/// Deliberately in-memory: losing grants on restart fails safe (an
/// unauthorized supersession stays impossible; the PM expert simply has to
/// escalate again). The single instance lives on `AppState` and rides into
/// MCP handlers via `ToolCallContext::pm_authorizations` — it must NOT be
/// created per call site or the grant issued by the answer route would be
/// invisible to the tool handler that checks it.
#[derive(Clone, Default)]
pub struct PmUserAuthorizations(Arc<Mutex<HashMap<String, u32>>>);

impl PmUserAuthorizations {
    /// Record that the user has authorized one decision change in `project_id`.
    pub fn grant(&self, project_id: &str) {
        let mut grants = self.0.lock().unwrap();
        *grants.entry(project_id.to_string()).or_insert(0) += 1;
    }

    /// Consume one outstanding grant for `project_id`. Returns `false` when
    /// none is outstanding — the caller must reject the change.
    pub fn consume(&self, project_id: &str) -> bool {
        let mut grants = self.0.lock().unwrap();
        match grants.get_mut(project_id) {
            Some(n) if *n > 0 => {
                *n -= 1;
                if *n == 0 {
                    grants.remove(project_id);
                }
                true
            }
            _ => false,
        }
    }
}

/// Deterministic id for a project's PM expert, derived from the project id
/// so it is stable across restarts and idempotent ensures.
pub fn project_pm_expert_id(project_id: &str) -> String {
    format!("pm-expert-project-{project_id}")
}

/// System prompt appended whenever the PM expert takes a turn (see
/// `SessionManager::send_message_locked`, which derives it from the session
/// row's `expert_kind`), stating its role and hard rules.
pub const PM_EXPERT_SYSTEM_PROMPT: &str = r#"
# You Are This Project's PM Expert

You are the project's long-lived PM (project management) expert — the durable store of project-direction and business-logic decisions the user has made.

Your role and hard rules:
- Accumulate and remember the user's decisions about product direction, scope, and business logic for this project.
- Answer questions from worker sessions out of the decisions you already know — including judgment calls such as "would this change violate an existing decision?".
- When a question touches a decision you do NOT have recorded, do not guess: escalate it to the user by calling `mcp__peckboard__pm_escalate_to_user` with the question (and `asking_session_id` set to the consulting session, so you can relay the answer back once the user decides). The question becomes pending until the user answers; their answer is delivered to you as an express user decision.
- NEVER change, reverse, or reinterpret a recorded decision without express user authorization. Decisions belong to the user; you are their memory, not their replacement.
- When a user answer to an escalation changes an existing decision, record the change by calling `mcp__peckboard__pm_record_decision` with `supersedes_decision_id` — that is authorized only on the strength of that answer. Then relay the answer to the asking session.
- When another session consults you (an "Expert consultation request"), reply by calling `mcp__peckboard__ask_expert` with `reply_to_session_id` set to the asking session and `answer` set to your reply.
"#;

/// File name used for every project's PM-decision export within its folder.
pub const PM_DECISIONS_FILE: &str = "decisions.md";

/// Stable report-folder name for a project's PM-decision export, mirroring
/// the question expert's Q&A export scheme (see [`crate::service::qa_report`]):
/// a well-known location under `<data_dir>/reports/` so rehydration can find
/// it deterministically and it lists through the normal report surfaces. The
/// id is sanitized to the `[A-Za-z0-9_-]` charset the report routes accept as
/// a path segment (project ids are UUIDs, so this is a no-op in practice).
pub fn pm_decisions_folder(project_id: &str) -> String {
    let safe: String = project_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("pm-decisions-project-{safe}")
}

fn pm_decisions_path(data_dir: &Path, project_id: &str) -> PathBuf {
    data_dir
        .join("reports")
        .join(pm_decisions_folder(project_id))
        .join(PM_DECISIONS_FILE)
}

/// Render and write the project's PM-decision log to its durable export
/// file, regenerating the whole file from the CRUD layer (the DB is the
/// source of truth — unlike the Q&A export, this is a rewrite, not an
/// append). Returns the `(folder, file)` it landed in.
///
/// This is THE persistence hook: call it after every decision mutation
/// (record / answer / supersede) and from the bootstrap path so the file
/// always exists and stays current. Rehydration after a restart reads it
/// back (see [`rehydrate_pm_expert`]).
pub async fn export_pm_decisions(
    db: &Db,
    data_dir: &Path,
    project_id: &str,
) -> anyhow::Result<(String, String)> {
    let Some(project) = db.get_project(project_id).await? else {
        anyhow::bail!("project not found: {project_id}");
    };
    let decisions = db.list_pm_decisions_for_project(project_id).await?;

    let answered: Vec<&PmDecision> = decisions
        .iter()
        .filter(|d| d.status == "answered")
        .collect();
    let pending: Vec<&PmDecision> = decisions.iter().filter(|d| d.status == "pending").collect();
    let superseded: Vec<&PmDecision> = decisions
        .iter()
        .filter(|d| d.status == "superseded")
        .collect();

    let mut body = String::from("# PM Decision Log\n\n");
    body.push_str(
        "Durable export of this project's PM decisions, regenerated from \
         the decision log on every change.\n\n",
    );

    body.push_str("## Answered Decisions\n\n");
    if answered.is_empty() {
        body.push_str("_No decisions recorded yet._\n");
    } else {
        for d in &answered {
            let answer = d.answer.as_deref().unwrap_or("");
            let date = d.answered_at.as_deref().unwrap_or(&d.created_at);
            body.push_str(&format!(
                "- **{}** — {answer} _(decided {date})_\n",
                d.question
            ));
        }
    }

    body.push_str("\n## Pending Questions\n\n");
    if pending.is_empty() {
        body.push_str("_No pending questions._\n");
    } else {
        for d in &pending {
            body.push_str(&format!(
                "- **{}** _(asked {})_\n",
                d.question, d.created_at
            ));
        }
    }

    // Omitted entirely when empty rather than rendering a bare heading.
    if !superseded.is_empty() {
        body.push_str("\n## Superseded Decisions\n\n");
        for d in &superseded {
            let old_answer = d.answer.as_deref().unwrap_or("");
            let replacement = d
                .superseded_by
                .as_deref()
                .and_then(|id| decisions.iter().find(|r| r.id == id));
            let replaced_by = match replacement {
                Some(r) => format!("{} — {}", r.question, r.answer.as_deref().unwrap_or("")),
                None => d.superseded_by.clone().unwrap_or_default(),
            };
            body.push_str(&format!(
                "- **{}** — {old_answer} _(superseded by: {replaced_by})_\n",
                d.question
            ));
        }
    }

    let now = chrono::Utc::now().to_rfc3339();
    let expert_id = project_pm_expert_id(project_id);
    // Frontmatter kept compatible with the existing report readers, same
    // shape as the Q&A export (see `qa_report::append_qa_entry`).
    let content = format!(
        "---\ntitle: \"PM Decision Log ({name})\"\ndate: \"{now}\"\n\
         sessionId: \"{expert_id}\"\nprojectName: \"{name}\"\n---\n\n{body}",
        name = project.name,
    );

    let folder = pm_decisions_folder(project_id);
    let dir = data_dir.join("reports").join(&folder);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(PM_DECISIONS_FILE), content)?;
    Ok((folder, PM_DECISIONS_FILE.to_string()))
}

/// Feed the user's answer to an escalated PM question back into the flow.
/// This is THE user-answer seam the HTTP answer route calls, mirroring
/// [`crate::service::question_expert::record_user_answer`]:
///
/// 1. marks the pending question answered ([`Db::answer_question`] — fails
///    if the row is missing or no longer pending),
/// 2. grants a one-shot user authorization for the project, so the PM
///    expert's follow-up `pm_record_decision` call may set
///    `supersedes_decision_id` — the ONLY path that may change a recorded
///    decision,
/// 3. delivers the answer into the PM expert session the same way a user
///    message arrives (persist a `user` event + broadcast, then resume via
///    the dispatcher when live; `resume` is `None` headless / in tests),
/// 4. regenerates the durable decision export.
///
/// Returns the answered decision row.
pub async fn deliver_pm_user_answer(
    db: &Db,
    broadcaster: &Arc<Broadcaster>,
    data_dir: &Path,
    resume: Option<&Arc<dyn ExpertDispatcher>>,
    authorizations: &PmUserAuthorizations,
    decision_id: &str,
    answer: &str,
) -> anyhow::Result<PmDecision> {
    let answered = db.answer_question(decision_id, answer).await?;
    let project_id = answered.project_id.clone();

    // The PM expert is lazily ensured so delivery works even on DBs whose
    // bootstrap predates the PM-expert feature.
    let project = db
        .get_project(&project_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("project not found: {project_id}"))?;
    let expert = ensure_project_pm_expert(db, &project).await?;

    authorizations.grant(&project_id);

    let relay = match answered.asked_by_session_id.as_deref() {
        Some(asker) => format!(
            "\n\nThe question was originally raised by session {asker}. Relay this answer \
             to it by calling `ask_expert` with `reply_to_session_id` set to \"{asker}\" \
             and `answer` carrying the user's decision."
        ),
        None => String::new(),
    };
    let message = format!(
        "[User answer to escalated PM question — express user decision] (recorded by \
         Peckboard from the user's answer)\n\n\
         Question: {question}\n\
         Answer: {answer}\n\n\
         This decision is already recorded in the decision log (id {id}). If it changes \
         an existing decision, you are authorized — on the strength of this answer only — \
         to call `pm_record_decision` with `supersedes_decision_id` set to the decision it \
         replaces.{relay}",
        question = answered.question,
        id = answered.id,
    );

    if let Err(e) = crate::service::delivery::persist_user_message(
        db,
        broadcaster,
        &expert.id,
        &message,
        "pm-user-answer",
    )
    .await
    {
        tracing::warn!(expert_id = %expert.id, "failed to persist PM user-answer delivery: {e}");
    }

    if let Some(dispatcher) = resume
        && let Err(e) = dispatcher.resume_session(&expert.id, &message).await
    {
        tracing::warn!(expert_id = %expert.id, "failed to resume PM expert after answer: {e}");
    }

    if let Err(e) = export_pm_decisions(db, data_dir, &project_id).await {
        tracing::warn!(project_id = %project_id, "failed to re-export PM decisions: {e}");
    }

    Ok(answered)
}

/// Rehydrate a PM expert from its durable decision export so a fresh
/// session under its stable id resumes knowing every recorded decision —
/// the same bootstrap mechanism as
/// [`crate::service::question_expert::rehydrate_question_expert`].
///
/// Idempotent across repeated boots: each delivery records the export's
/// length in the marker event, and a subsequent call with an unchanged
/// export is a no-op. A project with no recorded decisions is skipped —
/// there is nothing worth seeding. Returns `true` when it delivered a
/// fresh bootstrap.
pub async fn rehydrate_pm_expert(
    db: &Db,
    broadcaster: &Arc<Broadcaster>,
    data_dir: &Path,
    expert: &Session,
) -> anyhow::Result<bool> {
    let Some(project_id) = expert.project_id.as_deref() else {
        return Ok(false);
    };
    if db
        .list_pm_decisions_for_project(project_id)
        .await?
        .is_empty()
    {
        return Ok(false);
    }

    let export = match std::fs::read_to_string(pm_decisions_path(data_dir, project_id)) {
        Ok(content) => {
            let body = crate::service::qa_report::strip_frontmatter(&content);
            let trimmed = body.trim().to_string();
            if trimmed.is_empty() {
                return Ok(false);
            }
            trimmed
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };

    let export_len = export.len() as i64;

    // Skip if we already rehydrated this exact export into this session.
    let events = db.list_events_by_session(&expert.id, None).await?;
    let already = events.iter().rev().any(|e| {
        e.kind == "user"
            && serde_json::from_str::<serde_json::Value>(&e.data)
                .ok()
                .map(|d| {
                    d.get("source").and_then(|v| v.as_str()) == Some("pm-rehydration")
                        && d.get("exportLen").and_then(|v| v.as_i64()) == Some(export_len)
                })
                .unwrap_or(false)
    });
    if already {
        return Ok(false);
    }

    let message = format!(
        "[Rehydration — your accumulated PM decisions] (NOT from the user — \
         restored by Peckboard from your decision export so you resume where \
         you left off)\n\nBelow is this project's decision log as you last \
         recorded it. Treat it as known context: answer worker consultations \
         and violation checks from these decisions, and escalate to the user \
         only for matters they don't cover. Never change a decision without \
         express user authorization.\n\n{export}"
    );

    // Persist as a user-style event and broadcast it the same way a user
    // message renders. Rehydration runs at early boot, before the
    // `SessionManager` exists, so there is no resume here — the expert
    // consumes this on its next run. The `exportLen` marker keeps this
    // idempotent across boots.
    let event = db
        .append_event(
            &expert.id,
            "user",
            serde_json::json!({
                "text": message,
                "source": "pm-rehydration",
                "exportLen": export_len,
            }),
        )
        .await?;

    broadcaster.broadcast(WsEvent {
        event_type: "event".into(),
        session_id: expert.id.clone(),
        data: serde_json::json!({
            "id": event.id,
            "seq": event.seq,
            "ts": event.ts,
            "kind": event.kind,
            "data": serde_json::from_str::<serde_json::Value>(&event.data).unwrap_or_default(),
        }),
    });

    Ok(true)
}

/// Idempotently get-or-create the PM expert owned by `project`. Wired into
/// every place the project question-expert is ensured (project creation,
/// startup backfill, expert spin-up), so existing projects gain a PM expert
/// on upgrade. Re-running never clobbers the accumulated row.
pub async fn ensure_project_pm_expert(db: &Db, project: &Project) -> anyhow::Result<Session> {
    let now = chrono::Utc::now().to_rfc3339();
    let expert = db
        .upsert_permanent_expert(NewSession {
            id: project_pm_expert_id(&project.id),
            name: format!("PM Expert ({})", project.name),
            folder_id: project.folder_id.clone(),
            model: project.model.clone(),
            effort: project.effort.clone(),
            project_id: Some(project.id.clone()),
            is_expert: true,
            expert_kind: Some("pm".into()),
            is_permanent: true,
            knowledge_area: Some("Project direction & decisions (PM)".into()),
            knowledge_summary: Some(
                "I am this project's PM expert — the durable store of \
                 project-direction and business-logic decisions. Workers ask \
                 me whether a change conflicts with a known decision; when I \
                 don't have one recorded, the question escalates to the user. \
                 I never change a decision without express user authorization."
                    .into(),
            ),
            created_at: now.clone(),
            last_activity: now,
            ..Default::default()
        })
        .await?;
    Ok(expert)
}
