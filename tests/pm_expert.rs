//! Integration test for the per-project PM expert bootstrap.
//!
//! Asserts the locked design against the public API + an in-memory DB:
//! - creating a project (via the real MCP `create_project` path) yields a
//!   permanent PM expert under the stable id `pm-expert-project-<id>` with
//!   `expert_kind = "pm"`, visible through `list_experts`,
//! - the ensure helper is idempotent (no duplicate, no clobber),
//! - an existing project without a PM expert gains one on ensure (the
//!   startup-backfill shape for DBs that predate the feature).

use std::sync::Arc;

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewProject};
use peckboard::service::mcp_server::{McpToolRegistry, ToolCallContext};
use peckboard::service::pm_expert::{ensure_project_pm_expert, project_pm_expert_id};
use peckboard::ws::broadcaster::Broadcaster;

async fn seed_folder(db: &Db, folder_id: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: folder_id.into(),
        name: "F".into(),
        path: format!("/tmp/pm-expert/{folder_id}"),
        created_at: ts,
    })
    .await
    .unwrap();
}

async fn seed_project(db: &Db, project_id: &str, folder_id: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    seed_folder(db, folder_id).await;
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
async fn creating_a_project_bootstraps_its_pm_expert() {
    let db = Arc::new(Db::in_memory().unwrap());
    seed_folder(&db, "f1").await;

    let registry = McpToolRegistry::new();
    let result = registry
        .handle_tool_call(
            "create_project",
            serde_json::json!({ "name": "Proj", "folder_id": "f1" }),
            &ctx(&db, "chat-1"),
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "ok");
    let project_id = result["project"]["id"].as_str().unwrap().to_string();

    let expert = db
        .get_expert_session(&project_pm_expert_id(&project_id))
        .await
        .unwrap()
        .expect("project creation must bootstrap a PM expert");
    assert!(expert.is_expert);
    assert!(expert.is_permanent);
    assert_eq!(expert.expert_kind.as_deref(), Some("pm"));
    assert_eq!(expert.project_id.as_deref(), Some(project_id.as_str()));

    // Visible wherever experts are listed.
    let listed = registry
        .handle_tool_call(
            "list_experts",
            serde_json::json!({ "project_id": project_id }),
            &ctx(&db, "chat-1"),
        )
        .await
        .unwrap();
    let experts = listed["experts"].as_array().unwrap();
    assert!(
        experts.iter().any(|e| {
            e["session_id"] == project_pm_expert_id(&project_id).as_str()
                && e["expert_kind"] == "pm"
        }),
        "PM expert must appear in list_experts, got: {experts:?}"
    );
}

#[tokio::test]
async fn ensure_project_pm_expert_is_idempotent() {
    let db = Arc::new(Db::in_memory().unwrap());
    seed_project(&db, "p1", "f1").await;
    let project = db.get_project("p1").await.unwrap().unwrap();

    let first = ensure_project_pm_expert(&db, &project).await.unwrap();
    assert_eq!(first.id, project_pm_expert_id("p1"));

    // Re-running (same boot or a simulated restart) doesn't duplicate or
    // clobber the accumulated row.
    let second = ensure_project_pm_expert(&db, &project).await.unwrap();
    assert_eq!(second.id, first.id);
    let pm_experts: Vec<_> = db
        .list_expert_sessions_by_project("p1")
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.expert_kind.as_deref() == Some("pm"))
        .collect();
    assert_eq!(pm_experts.len(), 1, "exactly one PM expert per project");
}

#[tokio::test]
async fn existing_project_without_pm_expert_gains_one_on_ensure() {
    let db = Arc::new(Db::in_memory().unwrap());
    // Created directly via the DB, bypassing the create_project wiring —
    // the shape of a project that predates the PM-expert feature.
    seed_project(&db, "legacy", "f-legacy").await;
    assert!(
        db.list_expert_sessions_by_project("legacy")
            .await
            .unwrap()
            .is_empty(),
        "precondition: legacy project has no experts yet"
    );

    // The startup backfill loop runs ensure for every project.
    let project = db.get_project("legacy").await.unwrap().unwrap();
    let expert = ensure_project_pm_expert(&db, &project).await.unwrap();

    assert_eq!(expert.id, project_pm_expert_id("legacy"));
    assert!(expert.is_expert);
    assert!(expert.is_permanent);
    assert_eq!(expert.expert_kind.as_deref(), Some("pm"));
}
