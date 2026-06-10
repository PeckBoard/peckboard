//! End-to-end check of the PM-expert worker-prompt path: the PM expert
//! ensured for a project flows through `list_expert_sessions_by_scope`
//! (the orchestrator's expert source) into `build_worker_prompt`, and the
//! prompt carries the PM rules every worker must follow.

use peckboard::db::Db;
use peckboard::db::models::{NewCard, NewFolder, NewProject};
use peckboard::service::pm_expert;
use peckboard::worker::pipeline::build_worker_prompt;

#[tokio::test]
async fn worker_prompt_includes_pm_expert_and_rules() {
    let db = Db::in_memory().unwrap();
    let ts = chrono::Utc::now().to_rfc3339();

    db.create_folder(NewFolder {
        id: "f1".into(),
        name: "F".into(),
        path: "/tmp/worker-prompt-pm".into(),
        created_at: ts.clone(),
    })
    .await
    .unwrap();
    db.create_project(NewProject {
        id: "p1".into(),
        name: "P".into(),
        context: "ctx".into(),
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
        last_accessed_at: ts.clone(),
    })
    .await
    .unwrap();
    db.create_card(NewCard {
        id: "c1".into(),
        project_id: "p1".into(),
        title: "Add reporting".into(),
        description: "Build the report view.".into(),
        step: "in_progress".into(),
        priority: 1,
        workflow: "task".into(),
        model: None,
        effort: None,
        blocked: false,
        block_reason: None,
        created_at: ts.clone(),
        updated_at: ts,
    })
    .await
    .unwrap();

    let project = db.get_project("p1").await.unwrap().unwrap();
    pm_expert::ensure_project_pm_expert(&db, &project)
        .await
        .unwrap();

    // Same query spawn_worker_for_card uses to assemble in-scope experts.
    let experts = db.list_expert_sessions_by_scope("p1").await.unwrap();
    assert!(
        experts.iter().any(|e| e.id == "pm-expert-project-p1"),
        "PM expert must be in scope for the project's workers",
    );

    let card = db.get_card("c1").await.unwrap().unwrap();
    let steps: Vec<String> = vec!["backlog".into(), "in_progress".into(), "done".into()];
    let prompt = build_worker_prompt(&project, &card, &card.step, &steps, None, &experts, None);

    // The PM expert is referenced by its stable, addressable id.
    assert!(prompt.contains("pm-expert-project-p1"));
    // The four PM rules: check before direction/business-logic changes,
    // consult instead of guessing, record new decisions, user-only changes.
    assert!(prompt.contains("pm_check_decisions"));
    assert!(prompt.contains("do NOT guess and do NOT ask the user directly"));
    assert!(prompt.contains("pm_record_decision"));
    assert!(prompt.contains("decisions belong to the user"));
}
