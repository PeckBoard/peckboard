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

use crate::db::Db;
use crate::db::models::{NewSession, Project, Session};

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
- When a question touches a decision you do NOT have recorded, say so plainly and escalate it to the user as a pending question rather than guessing. (Until dedicated escalation tooling exists, state clearly that the matter needs a human decision so the asking session falls back to the user.)
- NEVER change, reverse, or reinterpret a recorded decision without express user authorization. Decisions belong to the user; you are their memory, not their replacement.
- When another session consults you (an "Expert consultation request"), reply by calling `mcp__peckboard__ask_expert` with `reply_to_session_id` set to the asking session and `answer` set to your reply.
"#;

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
