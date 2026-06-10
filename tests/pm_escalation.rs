//! Integration tests for the PM escalation and user-answer-to-decision
//! flow, against the public registry + an in-memory DB (no live agent /
//! dispatcher):
//! - the PM expert escalates via `pm_escalate_to_user` → a pending question
//!   exists, pending_count > 0, and the export shows it,
//! - a non-PM caller is rejected and directed to ask_expert "pm",
//! - a simulated user answer delivers the answer into the PM session,
//!   authorizes exactly one supersession, marks the question answered,
//!   brings pending_count back to 0, and updates the export.

use std::sync::Arc;

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewProject, NewSession};
use peckboard::service::mcp_server::{McpToolRegistry, ToolCallContext};
use peckboard::service::pm_expert::{
    PM_DECISIONS_FILE, PmUserAuthorizations, deliver_pm_user_answer, ensure_project_pm_expert,
    pm_decisions_folder, project_pm_expert_id,
};
use peckboard::ws::broadcaster::Broadcaster;

async fn seed_project(db: &Db, project_id: &str, folder_id: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: folder_id.into(),
        name: "F".into(),
        path: format!("/tmp/pm-escalation/{folder_id}"),
        created_at: ts.clone(),
    })
    .await
    .unwrap();
    db.create_project(NewProject {
        id: project_id.into(),
        name: "Project".into(),
        context: String::new(),
        folder_id: folder_id.into(),
        worker_count: 1,
        status: "active".into(),
        workflow: "task".into(),
        model: Some("mock:happy-path".into()),
        effort: None,
        parallel_instructions: false,
        auto_notify_changes: true,
        worker_communication: false,
        created_at: ts.clone(),
        last_accessed_at: ts.clone(),
    })
    .await
    .unwrap();
}

async fn seed_worker(db: &Db, id: &str, folder_id: &str, project_id: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_session(NewSession {
        id: id.into(),
        name: format!("session {id}"),
        folder_id: folder_id.into(),
        model: Some("mock:happy-path".into()),
        is_worker: true,
        project_id: Some(project_id.into()),
        created_at: ts.clone(),
        last_activity: ts,
        ..Default::default()
    })
    .await
    .unwrap();
}

fn ctx(
    db: &Arc<Db>,
    session_id: &str,
    project_id: Option<&str>,
    data_dir: Option<&std::path::Path>,
    authorizations: &PmUserAuthorizations,
) -> ToolCallContext {
    ToolCallContext {
        session_id: session_id.into(),
        project_id: project_id.map(|s| s.to_string()),
        card_id: None,
        db: db.clone(),
        broadcaster: Broadcaster::new(),
        provider_registry: None,
        expert_dispatcher: None,
        data_dir: data_dir.map(|p| p.to_path_buf()),
        pm_authorizations: authorizations.clone(),
    }
}

async fn event_texts(db: &Db, session_id: &str) -> Vec<String> {
    db.events_tail(session_id, 100)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| {
            serde_json::from_str::<serde_json::Value>(&e.data)
                .ok()
                .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(String::from))
        })
        .collect()
}

fn read_export(data_dir: &std::path::Path, project_id: &str) -> String {
    std::fs::read_to_string(
        data_dir
            .join("reports")
            .join(pm_decisions_folder(project_id))
            .join(PM_DECISIONS_FILE),
    )
    .unwrap()
}

#[tokio::test]
async fn escalation_then_user_answer_authorizes_one_supersession() {
    let db = Arc::new(Db::in_memory().unwrap());
    let data_dir = tempfile::tempdir().unwrap();
    let bc = Broadcaster::new();
    let auths = PmUserAuthorizations::default();
    seed_project(&db, "p1", "f1").await;
    seed_worker(&db, "worker-1", "f1", "p1").await;

    let project = db.get_project("p1").await.unwrap().unwrap();
    let pm = ensure_project_pm_expert(&db, &project).await.unwrap();
    assert_eq!(pm.id, project_pm_expert_id("p1"));

    // An existing decision the user's answer will later change.
    let old = db
        .record_decision("p1", "Currency handling", "Floats are fine.", None)
        .await
        .unwrap();

    // 1. The PM expert escalates a question it has no decision for.
    let registry = McpToolRegistry::new();
    let pm_ctx = ctx(&db, &pm.id, Some("p1"), Some(data_dir.path()), &auths);
    let result = registry
        .handle_tool_call(
            "pm_escalate_to_user",
            serde_json::json!({
                "question": "Should prices switch to integer cents?",
                "context": "Float rounding bugs reported in invoicing.",
                "asking_session_id": "worker-1",
            }),
            &pm_ctx,
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "ok");
    let pending_id = result["pending_question_id"].as_str().unwrap().to_string();
    assert_eq!(result["pending_count"], 1);

    // Pending row exists, count > 0, export shows the waiting question.
    assert_eq!(db.pending_pm_decision_count("p1").await.unwrap(), 1);
    let pending = db.list_pending_pm_decisions("p1").await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, pending_id);
    assert_eq!(pending[0].asked_by_session_id.as_deref(), Some("worker-1"));
    let export = read_export(data_dir.path(), "p1");
    assert!(
        export.contains("Should prices switch to integer cents?"),
        "export must list the pending question, got: {export}"
    );

    // Without a user answer, even the PM expert cannot supersede.
    let premature = registry
        .handle_tool_call(
            "pm_record_decision",
            serde_json::json!({
                "title": "Currency handling",
                "decision": "Integer cents only.",
                "supersedes_decision_id": old.id,
            }),
            &pm_ctx,
        )
        .await;
    let msg = premature.unwrap_err().to_string();
    assert!(
        msg.contains("user authorization"),
        "unauthorized supersession must be rejected, got: {msg}"
    );

    // 2. The user answers: pending → answered, answer delivered into the PM
    //    session, one supersession authorized, export refreshed.
    let answered = deliver_pm_user_answer(
        &db,
        &bc,
        data_dir.path(),
        None,
        &auths,
        &pending_id,
        "Yes — integer cents everywhere.",
    )
    .await
    .unwrap();
    assert_eq!(answered.status, "answered");
    assert_eq!(db.pending_pm_decision_count("p1").await.unwrap(), 0);

    let texts = event_texts(&db, &pm.id).await;
    assert!(
        texts.iter().any(|t| t.contains("express user decision")
            && t.contains("Should prices switch to integer cents?")
            && t.contains("Yes — integer cents everywhere.")
            && t.contains("worker-1")),
        "PM expert must receive the answer coupled with question + relay target, got: {texts:?}"
    );

    // 3. The PM expert now records the superseding decision successfully.
    let superseded = registry
        .handle_tool_call(
            "pm_record_decision",
            serde_json::json!({
                "title": "Currency handling",
                "decision": "Integer cents only.",
                "supersedes_decision_id": old.id,
            }),
            &pm_ctx,
        )
        .await
        .unwrap();
    assert_eq!(superseded["status"], "ok");
    assert_eq!(superseded["superseded_decision_id"], old.id.as_str());

    let active = db.list_answered_pm_decisions("p1").await.unwrap();
    assert!(
        !active.iter().any(|d| d.id == old.id),
        "the old decision must no longer be active"
    );
    let export = read_export(data_dir.path(), "p1");
    assert!(
        export.contains("Integer cents only.") && export.contains("Superseded Decisions"),
        "export must reflect the supersession, got: {export}"
    );

    // The authorization was one-shot: a second supersession is rejected.
    let again = registry
        .handle_tool_call(
            "pm_record_decision",
            serde_json::json!({
                "title": "Currency handling v3",
                "decision": "Back to floats.",
                "supersedes_decision_id": superseded["decision"]["id"].as_str().unwrap(),
            }),
            &pm_ctx,
        )
        .await;
    assert!(
        again.is_err(),
        "the user authorization must be consumed by the first supersession"
    );
}

#[tokio::test]
async fn pm_escalate_to_user_rejects_non_pm_callers() {
    let db = Arc::new(Db::in_memory().unwrap());
    let auths = PmUserAuthorizations::default();
    seed_project(&db, "p1", "f1").await;
    seed_worker(&db, "worker-1", "f1", "p1").await;

    let registry = McpToolRegistry::new();
    let err = registry
        .handle_tool_call(
            "pm_escalate_to_user",
            serde_json::json!({ "question": "Should we ship the beta this week?" }),
            &ctx(&db, "worker-1", Some("p1"), None, &auths),
        )
        .await;

    let msg = err.unwrap_err().to_string();
    assert!(
        msg.contains("ask_expert") && msg.contains("pm"),
        "rejection must direct the caller to the PM expert, got: {msg}"
    );
    assert_eq!(db.pending_pm_decision_count("p1").await.unwrap(), 0);
}

#[tokio::test]
async fn pm_escalate_to_user_rejects_cross_project_asking_session() {
    let db = Arc::new(Db::in_memory().unwrap());
    let auths = PmUserAuthorizations::default();
    seed_project(&db, "p1", "f1").await;
    seed_project(&db, "p2", "f2").await;
    seed_worker(&db, "outsider", "f2", "p2").await;

    let project = db.get_project("p1").await.unwrap().unwrap();
    let pm = ensure_project_pm_expert(&db, &project).await.unwrap();

    let registry = McpToolRegistry::new();
    let err = registry
        .handle_tool_call(
            "pm_escalate_to_user",
            serde_json::json!({
                "question": "Anything",
                "asking_session_id": "outsider",
            }),
            &ctx(&db, &pm.id, Some("p1"), None, &auths),
        )
        .await;
    assert!(
        err.is_err(),
        "an asking session from another project must be rejected"
    );
    assert_eq!(db.pending_pm_decision_count("p1").await.unwrap(), 0);
}
