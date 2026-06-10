//! The "question expert" — a special long-lived expert a session consults
//! BEFORE bothering the human. There is exactly one stable GLOBAL
//! question-expert (handles chat-session questions) plus one per project.
//!
//! LOCKED DESIGN:
//! - Question-experts have `expert_kind = "question"`, `is_permanent = true`,
//!   and a STABLE/deterministic session id so they survive restarts and
//!   rehydrate under the same id. The id is derived from a fixed key for the
//!   global one and from `project_id` for per-project ones.
//! - Creation is idempotent: re-running `ensure_*` never clobbers an existing
//!   row (see [`Db::upsert_permanent_question_expert`]), so the accumulated
//!   session/Q&A survives a server restart or a repeated `spin_up_experts`.
//! - When the user answers a question, the resolved Q&A is fed BACK to the
//!   in-scope question-expert coupled with the original question context, so
//!   it can answer the same thing next time without troubling the human.

use std::path::Path;
use std::sync::Arc;

use crate::db::Db;
use crate::db::models::{NewFolder, NewSession, Project, Session};
use crate::ws::broadcaster::{Broadcaster, WsEvent};

/// Stable id of the one global question-expert. Deterministic so it
/// rehydrates under the same id across restarts.
pub const GLOBAL_QUESTION_EXPERT_ID: &str = "question-expert-global";

/// Folder the global question-expert is anchored to. It has no project, so
/// it gets a dedicated folder pointing at the data dir.
const GLOBAL_QUESTION_EXPERT_FOLDER_ID: &str = "question-expert-global-folder";

/// Deterministic id for a project's question-expert, derived from the
/// project id so it is stable across restarts and idempotent spin-ups.
pub fn project_question_expert_id(project_id: &str) -> String {
    format!("question-expert-project-{project_id}")
}

/// Idempotently get-or-create the global question-expert. Safe to call on
/// every startup: an existing row is returned untouched.
pub async fn ensure_global_question_expert(db: &Db, data_dir: &Path) -> anyhow::Result<Session> {
    let folder_id = ensure_global_folder(db, data_dir).await?;
    let now = chrono::Utc::now().to_rfc3339();
    let expert = db
        .upsert_permanent_question_expert(NewSession {
            id: GLOBAL_QUESTION_EXPERT_ID.into(),
            name: "Question Expert (Global)".into(),
            folder_id,
            project_id: None,
            is_expert: true,
            expert_kind: Some("question".into()),
            is_permanent: true,
            knowledge_area: Some("User Q&A (global)".into()),
            knowledge_summary: Some(
                "I am the global question-expert. Sessions consult me before \
                 asking the user. I accumulate the answers users give so the \
                 same question doesn't have to be asked twice."
                    .into(),
            ),
            created_at: now.clone(),
            last_activity: now,
            ..Default::default()
        })
        .await?;
    Ok(expert)
}

/// Idempotently get-or-create the question-expert owned by `project`. Called
/// when a project's experts are spun up; re-running never clobbers the
/// accumulated row.
pub async fn ensure_project_question_expert(db: &Db, project: &Project) -> anyhow::Result<Session> {
    let now = chrono::Utc::now().to_rfc3339();
    let expert = db
        .upsert_permanent_question_expert(NewSession {
            id: project_question_expert_id(&project.id),
            name: format!("Question Expert ({})", project.name),
            folder_id: project.folder_id.clone(),
            model: project.model.clone(),
            effort: project.effort.clone(),
            project_id: Some(project.id.clone()),
            is_expert: true,
            expert_kind: Some("question".into()),
            is_permanent: true,
            knowledge_area: Some("User Q&A (project)".into()),
            knowledge_summary: Some(
                "I am this project's question-expert. Workers consult me before \
                 asking the user. I accumulate the project-specific answers users \
                 give so the same question doesn't have to be asked twice."
                    .into(),
            ),
            created_at: now.clone(),
            last_activity: now,
            ..Default::default()
        })
        .await?;
    Ok(expert)
}

/// The question-expert a session in `project_id` should consult: the
/// project's own question-expert when present, otherwise the global one.
/// An unscoped (no-project) caller resolves to the global question-expert.
pub async fn in_scope_question_expert(
    db: &Db,
    project_id: Option<&str>,
) -> anyhow::Result<Option<Session>> {
    if let Some(pid) = project_id {
        let id = project_question_expert_id(pid);
        if let Some(e) = db.get_expert_session(&id).await? {
            return Ok(Some(e));
        }
    }
    db.get_expert_session(GLOBAL_QUESTION_EXPERT_ID).await
}

/// Feed a resolved user Q&A back to the in-scope question-expert, coupled
/// with the original question context, via the same async deliver mechanism
/// other expert traffic uses (append a `user` event + broadcast a
/// `worker-stdin-deliver`). Returns the question-expert id it delivered to,
/// or `None` when no question-expert is in scope. A no-op for self-delivery
/// (the question-expert never feeds answers to itself).
///
/// The Q&A is ALSO persisted EAGERLY to the scope's durable Q&A export file
/// under `data_dir` (see [`crate::service::qa_report`]) so it survives a
/// context reset / restart: the export IS what a fresh session rehydrates
/// from (see [`rehydrate_question_expert`]). The export scope follows the
/// resolved expert (its `project_id`), so a per-project answer lands in that
/// project's export and a global one in the global export.
pub async fn record_user_answer(
    db: &Db,
    broadcaster: &Arc<Broadcaster>,
    data_dir: &Path,
    project_id: Option<&str>,
    qa_context: &str,
) -> anyhow::Result<Option<String>> {
    let Some(expert) = in_scope_question_expert(db, project_id).await? else {
        return Ok(None);
    };

    // Persist to the durable Q&A export first, keyed off the expert's own
    // scope (which may differ from `project_id` when we fell back to the
    // global expert). Best-effort: a disk hiccup must not drop the in-memory
    // feedback delivery below.
    let project_name = match expert.project_id.as_deref() {
        Some(pid) => db.get_project(pid).await.ok().flatten().map(|p| p.name),
        None => None,
    };
    if let Err(e) = crate::service::qa_report::append_qa_entry(
        data_dir,
        expert.project_id.as_deref(),
        project_name.as_deref(),
        &expert.id,
        qa_context,
        &chrono::Utc::now().to_rfc3339(),
    ) {
        tracing::warn!(expert_id = %expert.id, "failed to persist Q&A export: {e}");
    }

    let message = format!(
        "[User Q&A captured — learn this so you can answer it next time] \
         (NOT from the user — recorded by Peckboard)\n\n{qa_context}"
    );

    let _ = db
        .append_event(
            &expert.id,
            "user",
            serde_json::json!({
                "text": message,
                "source": "question-expert-feedback",
            }),
        )
        .await;

    broadcaster.broadcast(WsEvent {
        event_type: "worker-stdin-deliver".into(),
        session_id: expert.id.clone(),
        data: serde_json::json!({ "text": message }),
    });

    Ok(Some(expert.id))
}

/// Rehydrate a question-expert from its durable Q&A export so a fresh
/// session under its stable id resumes with everything it had previously
/// learned. Reads the scope's export (keyed off `expert.project_id`;
/// `None` → the global export) and, when present, delivers the rehydration
/// bootstrap as a `user` deliver-event the (re)spawned session consumes on
/// its next turn.
///
/// Idempotent across repeated boots: each delivery records the export's
/// length in the marker event, and a subsequent call with an unchanged
/// export is a no-op — so wiring this right after `ensure_*` at every
/// startup never double-seeds. Returns `true` when it delivered a fresh
/// bootstrap.
pub async fn rehydrate_question_expert(
    db: &Db,
    broadcaster: &Arc<Broadcaster>,
    data_dir: &Path,
    expert: &Session,
) -> anyhow::Result<bool> {
    let Some(export) =
        crate::service::qa_report::read_qa_export(data_dir, expert.project_id.as_deref())?
    else {
        return Ok(false);
    };

    let export_len = export.len() as i64;

    // Skip if we already rehydrated this exact export into this session.
    let events = db.list_events_by_session(&expert.id, None).await?;
    let already = events.iter().rev().any(|e| {
        e.kind == "user"
            && serde_json::from_str::<serde_json::Value>(&e.data)
                .ok()
                .map(|d| {
                    d.get("source").and_then(|v| v.as_str()) == Some("qa-rehydration")
                        && d.get("exportLen").and_then(|v| v.as_i64()) == Some(export_len)
                })
                .unwrap_or(false)
    });
    if already {
        return Ok(false);
    }

    let message = crate::service::qa_report::build_rehydration_prompt(&export);

    let _ = db
        .append_event(
            &expert.id,
            "user",
            serde_json::json!({
                "text": message,
                "source": "qa-rehydration",
                "exportLen": export_len,
            }),
        )
        .await;

    broadcaster.broadcast(WsEvent {
        event_type: "worker-stdin-deliver".into(),
        session_id: expert.id.clone(),
        data: serde_json::json!({ "text": message }),
    });

    Ok(true)
}

/// Ensure a folder for the global question-expert exists and return its id.
/// Reuses an existing folder that already points at the data dir (e.g. on a
/// second startup), otherwise creates a dedicated one.
async fn ensure_global_folder(db: &Db, data_dir: &Path) -> anyhow::Result<String> {
    if db
        .get_folder(GLOBAL_QUESTION_EXPERT_FOLDER_ID)
        .await?
        .is_some()
    {
        return Ok(GLOBAL_QUESTION_EXPERT_FOLDER_ID.to_string());
    }

    let path = data_dir.to_string_lossy().to_string();
    // The path column is UNIQUE; if some other folder already claims the data
    // dir path, reuse it rather than failing on the unique constraint.
    if let Some(existing) = db
        .list_folders()
        .await?
        .into_iter()
        .find(|f| f.path == path)
    {
        return Ok(existing.id);
    }

    let now = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: GLOBAL_QUESTION_EXPERT_FOLDER_ID.into(),
        name: "Question Expert".into(),
        path,
        created_at: now,
    })
    .await?;
    Ok(GLOBAL_QUESTION_EXPERT_FOLDER_ID.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn mk_db() -> Db {
        Db::in_memory().unwrap()
    }

    #[tokio::test]
    async fn global_question_expert_is_stable_and_idempotent() {
        let db = mk_db().await;
        let data_dir = std::path::PathBuf::from("/tmp/peckboard-test");

        let first = ensure_global_question_expert(&db, &data_dir).await.unwrap();
        assert_eq!(first.id, GLOBAL_QUESTION_EXPERT_ID);
        assert!(first.is_expert);
        assert!(first.is_permanent);
        assert_eq!(first.expert_kind.as_deref(), Some("question"));
        assert!(first.project_id.is_none());

        // Idempotent: a second call returns the same row, doesn't duplicate.
        let second = ensure_global_question_expert(&db, &data_dir).await.unwrap();
        assert_eq!(second.id, GLOBAL_QUESTION_EXPERT_ID);
        let experts = db.list_expert_sessions().await.unwrap();
        assert_eq!(experts.len(), 1);
    }

    #[tokio::test]
    async fn project_question_expert_scoped_and_idempotent() {
        let db = mk_db().await;
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f1".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        let project = db
            .create_project(crate::db::models::NewProject {
                id: "p1".into(),
                name: "Proj".into(),
                context: "".into(),
                folder_id: "f1".into(),
                worker_count: 1,
                status: "active".into(),
                workflow: "task".into(),
                model: None,
                effort: None,
                parallel_instructions: false,
                auto_notify_changes: true,
                worker_communication: false,
                created_at: ts.clone(),
                last_accessed_at: ts,
            })
            .await
            .unwrap();

        let e = ensure_project_question_expert(&db, &project).await.unwrap();
        assert_eq!(e.id, project_question_expert_id("p1"));
        assert_eq!(e.project_id.as_deref(), Some("p1"));
        assert_eq!(e.expert_kind.as_deref(), Some("question"));

        // Idempotent.
        ensure_project_question_expert(&db, &project).await.unwrap();
        let by_project = db.list_expert_sessions_by_project("p1").await.unwrap();
        assert_eq!(by_project.len(), 1);
    }

    #[tokio::test]
    async fn in_scope_prefers_project_then_global() {
        let db = mk_db().await;
        let data_dir = std::path::PathBuf::from("/tmp/peckboard-test");
        ensure_global_question_expert(&db, &data_dir).await.unwrap();

        // No project expert yet → falls back to global.
        let resolved = in_scope_question_expert(&db, Some("p1")).await.unwrap();
        assert_eq!(resolved.unwrap().id, GLOBAL_QUESTION_EXPERT_ID);

        // Create the project expert → now preferred.
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f1".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        let project = db
            .create_project(crate::db::models::NewProject {
                id: "p1".into(),
                name: "Proj".into(),
                context: "".into(),
                folder_id: "f1".into(),
                worker_count: 1,
                status: "active".into(),
                workflow: "task".into(),
                model: None,
                effort: None,
                parallel_instructions: false,
                auto_notify_changes: true,
                worker_communication: false,
                created_at: ts.clone(),
                last_accessed_at: ts,
            })
            .await
            .unwrap();
        ensure_project_question_expert(&db, &project).await.unwrap();
        let resolved = in_scope_question_expert(&db, Some("p1")).await.unwrap();
        assert_eq!(resolved.unwrap().id, project_question_expert_id("p1"));
    }

    #[tokio::test]
    async fn record_user_answer_delivers_with_context() {
        let db = mk_db().await;
        let data_dir = std::path::PathBuf::from("/tmp/peckboard-test");
        let expert = ensure_global_question_expert(&db, &data_dir).await.unwrap();
        let bc = Broadcaster::new();
        let export_dir = tempfile::tempdir().unwrap();

        let delivered = record_user_answer(
            &db,
            &bc,
            export_dir.path(),
            None,
            "**Which database?**: PostgreSQL",
        )
        .await
        .unwrap();
        assert_eq!(delivered.as_deref(), Some(GLOBAL_QUESTION_EXPERT_ID));

        // The Q&A landed on the question-expert's event log as a user event,
        // coupled with the question context.
        let events = db.list_events_by_session(&expert.id, None).await.unwrap();
        let user_ev = events
            .iter()
            .find(|e| e.kind == "user")
            .expect("question-expert should have received a user event");
        assert!(user_ev.data.contains("Which database?"));
        assert!(user_ev.data.contains("PostgreSQL"));
        assert!(user_ev.data.contains("question-expert-feedback"));
    }

    #[tokio::test]
    async fn record_user_answer_noop_without_expert() {
        let db = mk_db().await;
        let bc = Broadcaster::new();
        let export_dir = tempfile::tempdir().unwrap();
        let delivered = record_user_answer(&db, &bc, export_dir.path(), None, "q: a")
            .await
            .unwrap();
        assert!(delivered.is_none());
    }
}
