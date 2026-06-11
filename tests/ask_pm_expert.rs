//! Integration test for routing `ask_expert` to the per-project PM expert.
//!
//! Asserts the async contract against the public registry + an in-memory DB:
//! - the "pm" shorthand resolves to the stable `pm-expert-project-<id>`,
//!   lazily ensuring the expert when the row is absent,
//! - the question lands as a `user` event on the PM expert session,
//! - a PM reply (reply-mode) routes back to the asking worker session.

use std::sync::Arc;

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewProject, NewSession};
use peckboard::service::mcp_server::{McpToolRegistry, ToolCallContext};
use peckboard::service::pm_expert::project_pm_expert_id;
use peckboard::ws::broadcaster::Broadcaster;

async fn seed_project(db: &Db, project_id: &str, folder_id: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: folder_id.into(),
        name: "F".into(),
        path: format!("/tmp/ask-pm-expert/{folder_id}"),
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

async fn seed_knowledge_expert(db: &Db, id: &str, folder_id: &str, project_id: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_session(NewSession {
        id: id.into(),
        name: format!("expert: {id}"),
        folder_id: folder_id.into(),
        model: Some("mock:happy-path".into()),
        project_id: Some(project_id.into()),
        is_expert: true,
        expert_kind: Some("knowledge".into()),
        knowledge_area: Some("authentication".into()),
        knowledge_summary: Some("Knowledge area: authentication".into()),
        scope_path: Some("src/auth".into()),
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
        data_dir: None,
        folder_id: "f1".into(),
        pm_authorizations: Default::default(),
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
async fn ask_expert_pm_shorthand_spawns_delivers_and_replies() {
    let db = Arc::new(Db::in_memory().unwrap());
    seed_project(&db, "p1", "f1").await;
    seed_worker(&db, "worker", "f1", "p1").await;

    let pm_id = project_pm_expert_id("p1");
    // No PM expert exists yet — the shorthand must lazily spawn it.
    assert!(db.get_expert_session(&pm_id).await.unwrap().is_none());

    let registry = McpToolRegistry::new();
    let result = registry
        .handle_tool_call(
            "ask_expert",
            serde_json::json!({
                "expert_id": "pm",
                "question": "Is dark mode decided?",
            }),
            &ctx(&db, "worker", Some("p1")),
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "ok");
    assert_eq!(result["expert_id"], pm_id.as_str());

    // The PM expert was spawned under its stable id with the right shape.
    let pm = db
        .get_expert_session(&pm_id)
        .await
        .unwrap()
        .expect("PM expert must be lazily spawned");
    assert_eq!(pm.expert_kind.as_deref(), Some("pm"));
    assert_eq!(pm.project_id.as_deref(), Some("p1"));
    assert!(pm.is_permanent);

    // The question landed as a consultation event on the PM expert session.
    let pm_events = event_texts(&db, &pm_id).await;
    assert!(
        pm_events
            .iter()
            .any(|t| t.contains("Is dark mode decided?") && t.contains("consultation")),
        "PM expert must receive the question as an event, got: {pm_events:?}"
    );

    // A second ask is idempotent: still exactly one PM expert for the project.
    registry
        .handle_tool_call(
            "ask_expert",
            serde_json::json!({"expert_id": "pm", "question": "And light mode?"}),
            &ctx(&db, "worker", Some("p1")),
        )
        .await
        .unwrap();
    let pm_experts: Vec<_> = db
        .list_expert_sessions_by_project("p1")
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.expert_kind.as_deref() == Some("pm"))
        .collect();
    assert_eq!(pm_experts.len(), 1, "lazy ensure must not duplicate");

    // The PM expert replies via the existing reply-mode mechanism; the
    // answer routes back to the asking worker coupled with the question.
    let reply = registry
        .handle_tool_call(
            "ask_expert",
            serde_json::json!({
                "reply_to_session_id": "worker",
                "answer": "Yes — dark mode was approved by the user.",
                "question": "Is dark mode decided?",
            }),
            &ctx(&db, &pm_id, Some("p1")),
        )
        .await
        .unwrap();
    assert_eq!(reply["status"], "ok");
    assert_eq!(reply["reply_to_session_id"], "worker");

    let worker_events = event_texts(&db, "worker").await;
    assert!(
        worker_events.iter().any(|t| {
            t.contains("Expert answer")
                && t.contains("dark mode was approved")
                && t.contains("Is dark mode decided?")
        }),
        "worker must receive the PM reply coupled with context, got: {worker_events:?}"
    );
}

#[tokio::test]
async fn ask_expert_pm_area_hint_resolves_to_pm_expert() {
    // The "pm" area hint must route to the PM expert (lazily ensured) even
    // when other experts exist in the project.
    let db = Arc::new(Db::in_memory().unwrap());
    seed_project(&db, "p1", "f1").await;
    seed_worker(&db, "worker", "f1", "p1").await;
    seed_knowledge_expert(&db, "expert-auth", "f1", "p1").await;

    let registry = McpToolRegistry::new();
    let result = registry
        .handle_tool_call(
            "ask_expert",
            serde_json::json!({
                "area": "pm",
                "question": "Does removing OAuth violate a decision?",
            }),
            &ctx(&db, "worker", Some("p1")),
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "ok");
    assert_eq!(result["expert_id"], project_pm_expert_id("p1").as_str());
}
