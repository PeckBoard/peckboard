//! Integration test for C10 bootstrap wiring.
//!
//! Asserts the feature works out-of-the-box:
//! - a fresh boot creates exactly one global question-expert under its
//!   stable id, and a second boot (simulated restart) does not duplicate it,
//! - creating a project through the real `create_project` MCP entry point
//!   idempotently gives that project exactly one question-expert.

use std::sync::Arc;

use peckboard::db::Db;
use peckboard::db::models::NewFolder;
use peckboard::service::mcp_server::{McpToolRegistry, ToolCallContext};
use peckboard::service::question_expert::{
    GLOBAL_QUESTION_EXPERT_ID, ensure_global_question_expert, project_question_expert_id,
};
use peckboard::ws::broadcaster::Broadcaster;

fn data_dir() -> std::path::PathBuf {
    std::path::PathBuf::from("/tmp/peckboard-expert-bootstrap-test")
}

fn ctx(db: &Arc<Db>, session_id: &str) -> ToolCallContext {
    ToolCallContext {
        session_id: session_id.into(),
        project_id: None,
        card_id: None,
        db: db.clone(),
        broadcaster: Broadcaster::new(),
        provider_registry: None,
        expert_dispatcher: None,
        data_dir: None,
        folder_id: "f1".into(),
        pm_authorizations: Default::default(),
    }
}

#[tokio::test]
async fn boot_creates_one_global_question_expert_and_second_boot_does_not_duplicate() {
    let db = Arc::new(Db::in_memory().unwrap());

    // First boot.
    let first = ensure_global_question_expert(&db, &data_dir())
        .await
        .unwrap();
    assert_eq!(first.id, GLOBAL_QUESTION_EXPERT_ID);
    assert!(first.is_permanent);
    assert_eq!(first.expert_kind.as_deref(), Some("question"));

    // Second boot (simulated restart) must not duplicate or clobber.
    ensure_global_question_expert(&db, &data_dir())
        .await
        .unwrap();

    let globals: Vec<_> = db
        .list_expert_sessions()
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.expert_kind.as_deref() == Some("question") && e.project_id.is_none())
        .collect();
    assert_eq!(
        globals.len(),
        1,
        "exactly one global question-expert across boots"
    );
    assert_eq!(globals[0].id, GLOBAL_QUESTION_EXPERT_ID);
}

#[tokio::test]
async fn creating_a_project_yields_exactly_one_per_project_question_expert() {
    let db = Arc::new(Db::in_memory().unwrap());
    ensure_global_question_expert(&db, &data_dir())
        .await
        .unwrap();

    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: "f1".into(),
        name: "F".into(),
        path: "/tmp/expert-bootstrap/f1".into(),
        created_at: ts,
    })
    .await
    .unwrap();

    // Create a project through the real MCP entry point (exercises the
    // bootstrap wiring, not just the helper).
    let registry = McpToolRegistry::new();
    let result = registry
        .handle_tool_call(
            "create_project",
            serde_json::json!({ "name": "Demo", "folder_id": "f1" }),
            &ctx(&db, "chat-1"),
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "ok");
    let project_id = result["project"]["id"].as_str().unwrap().to_string();

    let project_question_experts = |experts: Vec<peckboard::db::models::Session>| {
        experts
            .into_iter()
            .filter(|e| e.expert_kind.as_deref() == Some("question"))
            .collect::<Vec<_>>()
    };

    let after_create = project_question_experts(
        db.list_expert_sessions_by_project(&project_id)
            .await
            .unwrap(),
    );
    assert_eq!(
        after_create.len(),
        1,
        "creating a project yields exactly one per-project question-expert"
    );
    assert_eq!(after_create[0].id, project_question_expert_id(&project_id));
    assert!(after_create[0].is_permanent);
    assert_eq!(
        after_create[0].project_id.as_deref(),
        Some(project_id.as_str())
    );

    // Re-ensuring (e.g. spin_up_experts on the same project) is idempotent.
    let project = db.get_project(&project_id).await.unwrap().unwrap();
    peckboard::service::question_expert::ensure_project_question_expert(&db, &project)
        .await
        .unwrap();
    let after_reensure = project_question_experts(
        db.list_expert_sessions_by_project(&project_id)
            .await
            .unwrap(),
    );
    assert_eq!(
        after_reensure.len(),
        1,
        "re-ensuring must not duplicate the per-project question-expert"
    );
}

/// A project that predates the bootstrap wiring (created straight through the
/// DB with no question-expert) is healed at startup by the same idempotent
/// ensure the backfill loop runs, so the feature works out-of-the-box on an
/// existing DB.
#[tokio::test]
async fn startup_backfills_a_question_expert_for_a_preexisting_project() {
    use peckboard::db::models::NewProject;

    let db = Arc::new(Db::in_memory().unwrap());
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: "f1".into(),
        name: "F".into(),
        path: "/tmp/expert-bootstrap/backfill".into(),
        created_at: ts.clone(),
    })
    .await
    .unwrap();
    db.create_project(NewProject {
        id: "legacy".into(),
        name: "Legacy".into(),
        context: String::new(),
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

    // No question-expert yet (the project bypassed the create_project wiring).
    assert!(
        db.list_expert_sessions_by_project("legacy")
            .await
            .unwrap()
            .is_empty()
    );

    // Backfill (what the startup loop does per project).
    let project = db.get_project("legacy").await.unwrap().unwrap();
    peckboard::service::question_expert::ensure_project_question_expert(&db, &project)
        .await
        .unwrap();

    let experts: Vec<_> = db
        .list_expert_sessions_by_project("legacy")
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.expert_kind.as_deref() == Some("question"))
        .collect();
    assert_eq!(experts.len(), 1, "backfill heals a pre-existing project");
    assert_eq!(experts[0].id, project_question_expert_id("legacy"));
}
