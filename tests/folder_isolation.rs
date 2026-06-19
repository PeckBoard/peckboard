//! Folder-isolation integration tests for the MCP tool surface and the
//! folder-change endpoints.
//!
//! The boundary rule under test: a session in folder F may only see (or
//! act on) entities in F (or globally-scoped entities such as the global
//! question-expert). Project-, session-, expert-, report-, and
//! repeating-task-scoped tools must all enforce this. Folder-change
//! routes cancel every owned session before mutating the row so a
//! mid-flight agent never sees the new folder.

use std::sync::Arc;

use peckboard::db::Db;
use peckboard::db::crud::MoveFolderOutcome;
use peckboard::db::models::{NewFolder, NewProject, NewRepeatingTask, NewSession};
use peckboard::service::mcp_server::{McpToolRegistry, ToolCallContext};
use peckboard::ws::broadcaster::Broadcaster;

// ── Helpers ─────────────────────────────────────────────────────────

async fn seed_folder(db: &Db, id: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: id.into(),
        name: id.into(),
        path: format!("/tmp/iso/{id}"),
        created_at: ts,
    })
    .await
    .unwrap();
}

async fn seed_project(db: &Db, id: &str, folder_id: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_project(NewProject {
        id: id.into(),
        name: id.into(),
        context: "".into(),
        folder_id: folder_id.into(),
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
}

#[allow(clippy::too_many_arguments)]
async fn seed_session(
    db: &Db,
    id: &str,
    folder_id: &str,
    project_id: Option<&str>,
    is_worker: bool,
    is_expert: bool,
    expert_kind: Option<&str>,
) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_session(NewSession {
        id: id.into(),
        name: id.into(),
        folder_id: folder_id.into(),
        model: None,
        effort: None,
        is_worker,
        project_id: project_id.map(|s| s.to_string()),
        card_id: None,
        conversation_id: None,
        created_at: ts.clone(),
        last_activity: ts,
        is_expert,
        expert_kind: expert_kind.map(|s| s.to_string()),
        knowledge_summary: None,
        knowledge_area: None,
        scope_path: None,
        is_permanent: false,
        repeating_task_id: None,
    })
    .await
    .unwrap();
}

fn ctx(
    db: &Arc<Db>,
    session_id: &str,
    folder_id: &str,
    project_id: Option<&str>,
) -> ToolCallContext {
    ToolCallContext {
        session_id: session_id.into(),
        project_id: project_id.map(|s| s.to_string()),
        card_id: None,
        folder_id: folder_id.into(),
        db: db.clone(),
        broadcaster: Broadcaster::new(),
        provider_registry: None,
        data_dir: None,
    }
}

/// Standard two-folder setup: f1 has p1; f2 has p2.
async fn two_folders_two_projects(db: &Db) {
    seed_folder(db, "f1").await;
    seed_folder(db, "f2").await;
    seed_project(db, "p1", "f1").await;
    seed_project(db, "p2", "f2").await;
}

// ── list_folders / list_projects ────────────────────────────────────

#[tokio::test]
async fn list_folders_returns_only_caller_folder() {
    let db = Arc::new(Db::in_memory().unwrap());
    two_folders_two_projects(&db).await;
    seed_session(&db, "chat-f1", "f1", None, false, false, None).await;
    let registry = McpToolRegistry::new();

    let result = registry
        .handle_tool_call(
            "list_folders",
            serde_json::json!({}),
            &ctx(&db, "chat-f1", "f1", None),
        )
        .await
        .unwrap();

    assert_eq!(result["count"], 1);
    let folders = result["folders"].as_array().unwrap();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0]["id"], "f1");
}

#[tokio::test]
async fn list_projects_returns_only_caller_folder() {
    let db = Arc::new(Db::in_memory().unwrap());
    two_folders_two_projects(&db).await;
    seed_session(&db, "chat-f1", "f1", None, false, false, None).await;
    let registry = McpToolRegistry::new();

    let result = registry
        .handle_tool_call(
            "list_projects",
            serde_json::json!({}),
            &ctx(&db, "chat-f1", "f1", None),
        )
        .await
        .unwrap();

    let projects = result["projects"].as_array().unwrap();
    let ids: Vec<&str> = projects.iter().map(|p| p["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["p1"]);
}

// ── scope_project rejects foreign-folder projects ────────────────────

#[tokio::test]
async fn create_card_in_foreign_folder_project_is_rejected_as_not_found() {
    // A chat session in f1 must not be able to create cards in p2 (f2).
    let db = Arc::new(Db::in_memory().unwrap());
    two_folders_two_projects(&db).await;
    seed_session(&db, "chat-f1", "f1", None, false, false, None).await;
    let registry = McpToolRegistry::new();

    let err = registry
        .handle_tool_call(
            "create_card",
            serde_json::json!({
                "project_id": "p2",
                "title": "leak",
                "description": "should not be created",
            }),
            &ctx(&db, "chat-f1", "f1", None),
        )
        .await
        .unwrap_err();
    // "not found" framing — the caller must not learn that p2 exists.
    assert!(err.to_string().contains("not found"), "got: {err}");
    let cards = db.list_cards_by_project("p2").await.unwrap();
    assert!(cards.is_empty(), "no card should have been created in p2");
}

#[tokio::test]
async fn worker_token_cross_project_scope_check_uses_not_found_framing() {
    // The token is scoped to p1; targeting p2 must look like p2 doesn't exist.
    let db = Arc::new(Db::in_memory().unwrap());
    two_folders_two_projects(&db).await;
    seed_session(&db, "w1", "f1", Some("p1"), true, false, None).await;
    let registry = McpToolRegistry::new();

    let err = registry
        .handle_tool_call(
            "list_cards",
            serde_json::json!({ "project_id": "p2" }),
            &ctx(&db, "w1", "f1", Some("p1")),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not found"), "got: {err}");
}

// ── create_project / create_folder ──────────────────────────────────

#[tokio::test]
async fn create_project_rejects_explicit_foreign_folder_id() {
    let db = Arc::new(Db::in_memory().unwrap());
    two_folders_two_projects(&db).await;
    seed_session(&db, "chat-f1", "f1", None, false, false, None).await;
    let registry = McpToolRegistry::new();

    let err = registry
        .handle_tool_call(
            "create_project",
            serde_json::json!({
                "name": "leak-project",
                "folder_id": "f2",
            }),
            &ctx(&db, "chat-f1", "f1", None),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not found"), "got: {err}");
    // And nothing landed in f2 (still just p2).
    let in_f2 = db.list_projects_by_folder("f2").await.unwrap();
    assert_eq!(in_f2.len(), 1);
    assert_eq!(in_f2[0].id, "p2");
}

#[tokio::test]
async fn create_project_defaults_to_caller_folder_when_omitted() {
    let db = Arc::new(Db::in_memory().unwrap());
    two_folders_two_projects(&db).await;
    seed_session(&db, "chat-f1", "f1", None, false, false, None).await;
    let registry = McpToolRegistry::new();

    let result = registry
        .handle_tool_call(
            "create_project",
            serde_json::json!({ "name": "new-in-f1" }),
            &ctx(&db, "chat-f1", "f1", None),
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "ok");
    assert_eq!(result["project"]["folderId"], "f1");
}

#[tokio::test]
async fn create_folder_via_mcp_only_returns_caller_folder() {
    let db = Arc::new(Db::in_memory().unwrap());
    two_folders_two_projects(&db).await;
    seed_session(&db, "chat-f1", "f1", None, false, false, None).await;
    let registry = McpToolRegistry::new();

    // Lookup by the caller's own path returns it.
    let result = registry
        .handle_tool_call(
            "create_folder",
            serde_json::json!({ "name": "f1", "path": "/tmp/iso/f1" }),
            &ctx(&db, "chat-f1", "f1", None),
        )
        .await
        .unwrap();
    assert_eq!(result["folder"]["id"], "f1");

    // A different path is rejected; no new folder created.
    let before = db.list_folders().await.unwrap().len();
    let err = registry
        .handle_tool_call(
            "create_folder",
            serde_json::json!({ "name": "f3", "path": "/tmp/iso/f3" }),
            &ctx(&db, "chat-f1", "f1", None),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("restricted"), "got: {err}");
    let after = db.list_folders().await.unwrap().len();
    assert_eq!(before, after, "no new folder may be created via MCP");
}

// ── read_worker_session ─────────────────────────────────────────────

#[tokio::test]
async fn read_worker_session_cross_folder_is_not_found() {
    let db = Arc::new(Db::in_memory().unwrap());
    two_folders_two_projects(&db).await;
    seed_session(&db, "chat-f1", "f1", None, false, false, None).await;
    // Worker session in f2.
    seed_session(&db, "w-f2", "f2", Some("p2"), true, false, None).await;
    let registry = McpToolRegistry::new();

    let err = registry
        .handle_tool_call(
            "read_worker_session",
            serde_json::json!({ "session_id": "w-f2" }),
            &ctx(&db, "chat-f1", "f1", None),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not found"), "got: {err}");
}

// ── list_experts / ask_expert: cross-folder rejection ───────────────

#[tokio::test]
async fn move_session_refuses_worker_session() {
    let db = Arc::new(Db::in_memory().unwrap());
    two_folders_two_projects(&db).await;
    seed_session(&db, "w1", "f1", Some("p1"), true, false, None).await;

    let outcome = db.move_session_to_folder("w1", "f2").await.unwrap();
    assert!(matches!(outcome, MoveFolderOutcome::RefusedOwnedSession));
}

#[tokio::test]
async fn move_session_targets_missing_folder() {
    let db = Arc::new(Db::in_memory().unwrap());
    seed_folder(&db, "f1").await;
    seed_session(&db, "chat", "f1", None, false, false, None).await;

    let outcome = db.move_session_to_folder("chat", "nope").await.unwrap();
    assert!(matches!(outcome, MoveFolderOutcome::TargetMissing));
}

#[tokio::test]
async fn move_session_succeeds_for_chat_session() {
    let db = Arc::new(Db::in_memory().unwrap());
    seed_folder(&db, "f1").await;
    seed_folder(&db, "f2").await;
    seed_session(&db, "chat", "f1", None, false, false, None).await;
    // Pre-existing queued message — must be dropped by the move.
    db.upsert_queued_message(peckboard::db::models::NewQueuedMessage {
        session_id: "chat".into(),
        text: "pending text".into(),
        queued_at: chrono::Utc::now().to_rfc3339(),
        model: None,
        effort: None,
    })
    .await
    .unwrap();

    let outcome = db.move_session_to_folder("chat", "f2").await.unwrap();
    let session = match outcome {
        MoveFolderOutcome::Moved(s) => s,
        other => panic!("expected Moved, got: {other:?}"),
    };
    assert_eq!(session.folder_id, "f2");
    // Queued message wiped.
    assert!(db.get_queued_message("chat").await.unwrap().is_none());
}

#[tokio::test]
async fn move_project_drags_owned_sessions_along() {
    let db = Arc::new(Db::in_memory().unwrap());
    two_folders_two_projects(&db).await;
    // Two workers and one expert all in p1/f1.
    seed_session(&db, "w1", "f1", Some("p1"), true, false, None).await;
    seed_session(&db, "w2", "f1", Some("p1"), true, false, None).await;
    seed_session(&db, "e1", "f1", Some("p1"), false, true, Some("knowledge")).await;
    // An unrelated session in p2/f2 — must not move.
    seed_session(&db, "u1", "f2", Some("p2"), true, false, None).await;

    let outcome = db.move_project_to_folder("p1", "f2").await.unwrap();
    let report = match outcome {
        MoveFolderOutcome::Moved(r) => r,
        other => panic!("expected Moved, got: {other:?}"),
    };
    assert_eq!(report.previous_folder_id, "f1");
    assert_eq!(report.sessions_moved, 3);
    assert_eq!(report.project.folder_id, "f2");

    // Every owned session now in f2.
    for sid in ["w1", "w2", "e1"] {
        assert_eq!(
            db.get_session(sid).await.unwrap().unwrap().folder_id,
            "f2",
            "session {sid} should be in f2",
        );
    }
    // The unrelated session is untouched.
    assert_eq!(
        db.get_session("u1").await.unwrap().unwrap().folder_id,
        "f2", /* it was already in f2 */
        "unrelated session must be unchanged",
    );
}

#[tokio::test]
async fn move_repeating_task_updates_task_and_spawned_sessions() {
    let db = Arc::new(Db::in_memory().unwrap());
    seed_folder(&db, "f1").await;
    seed_folder(&db, "f2").await;
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_repeating_task(NewRepeatingTask {
        id: "t1".into(),
        name: "t1".into(),
        description: "".into(),
        folder_id: "f1".into(),
        prompt: "x".into(),
        schedule_kind: "interval".into(),
        schedule_value: r#"{"minutes":60}"#.into(),
        model: None,
        effort: None,
        enabled: true,
        next_run_at: None,
        last_run_at: None,
        created_at: ts.clone(),
        updated_at: ts.clone(),
    })
    .await
    .unwrap();
    // Spawn-style session attached to t1 in f1.
    db.create_session(NewSession {
        id: "spawn".into(),
        name: "spawn".into(),
        folder_id: "f1".into(),
        is_worker: false,
        created_at: ts.clone(),
        last_activity: ts,
        repeating_task_id: Some("t1".into()),
        ..Default::default()
    })
    .await
    .unwrap();

    let outcome = db.move_repeating_task_to_folder("t1", "f2").await.unwrap();
    let report = match outcome {
        MoveFolderOutcome::Moved(r) => r,
        other => panic!("expected Moved, got: {other:?}"),
    };
    assert_eq!(report.task.folder_id, "f2");
    assert_eq!(report.sessions_moved, 1);
    assert_eq!(
        db.get_session("spawn").await.unwrap().unwrap().folder_id,
        "f2",
    );
}
