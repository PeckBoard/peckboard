//! Integration test for the question-expert (consult-before-ask) flow.
//!
//! Asserts the locked design against the public API + an in-memory DB:
//! - the default GLOBAL question-expert exists under a stable id after the
//!   startup helper runs (and is idempotent),
//! - a per-project question-expert is created and scoped to its project,
//! - a resolved user answer is fed back to the in-scope question-expert
//!   coupled with the original question context,
//! - consulting the question-expert routes through the `ask_expert` MCP tool.

use std::sync::Arc;

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewProject, NewSession};
use peckboard::service::mcp_server::{McpToolRegistry, ToolCallContext};
use peckboard::service::question_expert::{
    GLOBAL_QUESTION_EXPERT_ID, ensure_global_question_expert, ensure_project_question_expert,
    in_scope_question_expert, project_question_expert_id, record_user_answer,
};
use peckboard::ws::broadcaster::Broadcaster;

fn data_dir() -> std::path::PathBuf {
    std::path::PathBuf::from("/tmp/peckboard-question-expert-test")
}

async fn seed_project(db: &Db, project_id: &str, folder_id: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: folder_id.into(),
        name: "F".into(),
        path: format!("/tmp/qe/{folder_id}"),
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
        last_accessed_at: ts,
    })
    .await
    .unwrap();
}

async fn seed_chat_session(db: &Db, id: &str, folder_id: &str, project_id: Option<&str>) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_session(NewSession {
        id: id.into(),
        name: format!("session {id}"),
        folder_id: folder_id.into(),
        model: Some("mock:happy-path".into()),
        project_id: project_id.map(|s| s.to_string()),
        is_worker: project_id.is_some(),
        created_at: ts.clone(),
        last_activity: ts,
        ..Default::default()
    })
    .await
    .unwrap();
}

fn ctx(db: &Arc<Db>, session_id: &str, project_id: Option<&str>) -> ToolCallContext {
    ToolCallContext {
        session_id: session_id.into(),
        project_id: project_id.map(|s| s.to_string()),
        card_id: None,
        db: db.clone(),
        broadcaster: Broadcaster::new(),
        provider_registry: None,
        expert_dispatcher: None,
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

#[tokio::test]
async fn global_question_expert_exists_with_stable_id_and_is_idempotent() {
    let db = Arc::new(Db::in_memory().unwrap());

    let expert = ensure_global_question_expert(&db, &data_dir())
        .await
        .unwrap();
    assert_eq!(expert.id, GLOBAL_QUESTION_EXPERT_ID);
    assert!(expert.is_expert);
    assert!(expert.is_permanent);
    assert_eq!(expert.expert_kind.as_deref(), Some("question"));
    assert!(expert.project_id.is_none());

    // Re-running (simulating a restart) doesn't duplicate or clobber.
    ensure_global_question_expert(&db, &data_dir())
        .await
        .unwrap();
    let experts = db.list_expert_sessions().await.unwrap();
    assert_eq!(experts.len(), 1, "global question-expert must be unique");
    assert_eq!(experts[0].id, GLOBAL_QUESTION_EXPERT_ID);
}

#[tokio::test]
async fn resolved_user_answer_is_fed_back_to_question_expert_with_context() {
    let db = Arc::new(Db::in_memory().unwrap());
    let expert = ensure_global_question_expert(&db, &data_dir())
        .await
        .unwrap();
    let bc = Broadcaster::new();

    let delivered = record_user_answer(
        &db,
        &bc,
        &data_dir(),
        None,
        "**Which package manager should I use?**: pnpm",
    )
    .await
    .unwrap();
    assert_eq!(delivered.as_deref(), Some(GLOBAL_QUESTION_EXPERT_ID));

    let texts = event_texts(&db, &expert.id).await;
    assert!(
        texts
            .iter()
            .any(|t| { t.contains("Which package manager should I use?") && t.contains("pnpm") }),
        "question-expert must receive the Q&A coupled with question context, got: {texts:?}"
    );
}

#[tokio::test]
async fn project_question_expert_is_scoped_and_consulted_via_ask_expert() {
    let db = Arc::new(Db::in_memory().unwrap());
    ensure_global_question_expert(&db, &data_dir())
        .await
        .unwrap();
    seed_project(&db, "p1", "f1").await;
    let project = db.get_project("p1").await.unwrap().unwrap();

    let pexpert = ensure_project_question_expert(&db, &project).await.unwrap();
    assert_eq!(pexpert.id, project_question_expert_id("p1"));
    assert_eq!(pexpert.project_id.as_deref(), Some("p1"));
    assert_eq!(pexpert.expert_kind.as_deref(), Some("question"));

    // In-scope resolution prefers the project question-expert over global.
    let resolved = in_scope_question_expert(&db, Some("p1")).await.unwrap();
    assert_eq!(resolved.unwrap().id, project_question_expert_id("p1"));

    // A worker in p1 consults the question-expert via the ask_expert tool.
    seed_chat_session(&db, "worker-1", "f1", Some("p1")).await;
    let registry = McpToolRegistry::new();
    let result = registry
        .handle_tool_call(
            "ask_expert",
            serde_json::json!({
                "expert_id": project_question_expert_id("p1"),
                "question": "What is the deploy target?",
            }),
            &ctx(&db, "worker-1", Some("p1")),
        )
        .await
        .unwrap();

    assert_eq!(result["status"], "ok");
    assert_eq!(result["expert_id"], project_question_expert_id("p1"));

    // The question landed on the question-expert as an event.
    let expert_events = event_texts(&db, &project_question_expert_id("p1")).await;
    assert!(
        expert_events
            .iter()
            .any(|t| t.contains("What is the deploy target?")),
        "question-expert must receive the consultation, got: {expert_events:?}"
    );
}

#[tokio::test]
async fn chat_session_consults_global_question_expert() {
    let db = Arc::new(Db::in_memory().unwrap());
    ensure_global_question_expert(&db, &data_dir())
        .await
        .unwrap();
    // A plain chat session: no project, not a worker.
    db.create_folder(NewFolder {
        id: "cf".into(),
        name: "Chat".into(),
        path: "/tmp/qe/chat".into(),
        created_at: chrono::Utc::now().to_rfc3339(),
    })
    .await
    .unwrap();
    seed_chat_session(&db, "chat-1", "cf", None).await;

    let registry = McpToolRegistry::new();
    // Unscoped caller (no project) resolves only globally-scoped experts.
    let result = registry
        .handle_tool_call(
            "ask_expert",
            serde_json::json!({ "question": "How do I name commits?" }),
            &ctx(&db, "chat-1", None),
        )
        .await
        .unwrap();

    assert_eq!(result["status"], "ok");
    assert_eq!(result["expert_id"], GLOBAL_QUESTION_EXPERT_ID);
}
