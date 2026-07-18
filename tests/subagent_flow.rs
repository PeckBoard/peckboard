//! Integration tests for the `spawn_subagent` MCP tool and the
//! completion-reporting half (`peckboard::subagent::claim_and_compose`).
//!
//! The dispatch step (`_dispatch_session` marker → route → provider) and
//! the delivery step (resume parent) ride machinery covered by
//! `mock_provider.rs` / `session_lifecycle.rs`; here we pin the DB
//! contract: row shape, guards, cap, idempotent claim, report text.

use std::sync::Arc;

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewSession};
use peckboard::service::mcp_server::{McpToolRegistry, ToolCallContext};
use peckboard::ws::broadcaster::Broadcaster;

async fn seed_folder(db: &Db, id: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: id.into(),
        name: id.into(),
        path: format!("/tmp/subagent-test/{id}"),
        created_at: ts,
    })
    .await
    .unwrap();
}

async fn seed_session(db: &Db, id: &str, folder_id: &str, parent: Option<&str>) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_session(NewSession {
        id: id.into(),
        name: id.into(),
        folder_id: folder_id.into(),
        model: Some("claude:claude-opus-4-8".into()),
        created_at: ts.clone(),
        last_activity: ts,
        parent_session_id: parent.map(|s| s.to_string()),
        expert_kind: parent.map(|_| "subagent".to_string()),
        is_expert: parent.is_some(),
        ..Default::default()
    })
    .await
    .unwrap();
}

fn ctx(db: &Arc<Db>, session_id: &str, folder_id: &str) -> ToolCallContext {
    ToolCallContext {
        session_id: session_id.into(),
        project_id: None,
        card_id: None,
        folder_id: folder_id.into(),
        db: db.clone(),
        broadcaster: Broadcaster::new(),
        provider_registry: None,
        data_dir: None,
    }
}

async fn spawn(
    registry: &McpToolRegistry,
    ctx: &ToolCallContext,
    name: &str,
) -> anyhow::Result<serde_json::Value> {
    registry
        .handle_tool_call(
            "spawn_subagent",
            serde_json::json!({ "name": name, "prompt": "map the repo layout" }),
            ctx,
        )
        .await
}

#[tokio::test]
async fn spawn_creates_child_with_parent_link_and_marker() {
    let db = Arc::new(Db::in_memory().unwrap());
    seed_folder(&db, "f1").await;
    seed_session(&db, "parent", "f1", None).await;
    let registry = McpToolRegistry::new();

    let result = spawn(&registry, &ctx(&db, "parent", "f1"), "scout")
        .await
        .unwrap();

    assert_eq!(result["status"], "ok");
    let child_id = result["subagent_session_id"].as_str().unwrap().to_string();

    // Marker for the route's dispatch step carries the child id + prompt.
    assert_eq!(result["_dispatch_session"]["session_id"], child_id.as_str());
    let dispatch_text = result["_dispatch_session"]["text"].as_str().unwrap();
    assert!(dispatch_text.contains("map the repo layout"));
    assert!(dispatch_text.contains("subagent \"scout\""));

    let child = db.get_session(&child_id).await.unwrap().unwrap();
    assert_eq!(child.parent_session_id.as_deref(), Some("parent"));
    assert_eq!(child.expert_kind.as_deref(), Some("subagent"));
    assert_eq!(child.name, "sub: scout");
    assert_eq!(child.folder_id, "f1");
    // Model inherited from the caller.
    assert_eq!(child.model.as_deref(), Some("claude:claude-opus-4-8"));
    assert!(!child.is_worker);

    // The prompt is persisted as the child's first user event.
    let events = db.events_tail(&child_id, 10).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "user");
    let data: serde_json::Value = serde_json::from_str(&events[0].data).unwrap();
    assert_eq!(data["source"], "subagent-spawn");
    assert_eq!(data["text"], dispatch_text);
}

#[tokio::test]
async fn subagents_cannot_spawn_subagents() {
    let db = Arc::new(Db::in_memory().unwrap());
    seed_folder(&db, "f1").await;
    seed_session(&db, "parent", "f1", None).await;
    seed_session(&db, "child", "f1", Some("parent")).await;
    let registry = McpToolRegistry::new();

    let err = spawn(&registry, &ctx(&db, "child", "f1"), "grandchild")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("subagents cannot spawn subagents"));
}

#[tokio::test]
async fn concurrent_cap_frees_up_after_completion_claim() {
    let db = Arc::new(Db::in_memory().unwrap());
    seed_folder(&db, "f1").await;
    seed_session(&db, "parent", "f1", None).await;
    let registry = McpToolRegistry::new();
    let pctx = ctx(&db, "parent", "f1");

    let mut first_child = None;
    for i in 0..peckboard::subagent::MAX_CONCURRENT_SUBAGENTS {
        let r = spawn(&registry, &pctx, &format!("s{i}")).await.unwrap();
        first_child.get_or_insert_with(|| r["subagent_session_id"].as_str().unwrap().to_string());
    }

    let err = spawn(&registry, &pctx, "one-too-many").await.unwrap_err();
    assert!(err.to_string().contains("subagent limit reached"));

    // Claiming one child's completion frees a slot.
    let ts = chrono::Utc::now().to_rfc3339();
    assert!(
        db.claim_subagent_completion(first_child.as_deref().unwrap(), &ts)
            .await
            .unwrap()
    );
    spawn(&registry, &pctx, "fits-now").await.unwrap();
}

#[tokio::test]
async fn claim_and_compose_reports_final_reply_once() {
    let db = Db::in_memory().unwrap();
    seed_folder(&db, "f1").await;
    seed_session(&db, "parent", "f1", None).await;
    seed_session(&db, "sub: scout", "f1", Some("parent")).await;

    db.append_event("sub: scout", "user", serde_json::json!({ "text": "task" }))
        .await
        .unwrap();
    db.append_event(
        "sub: scout",
        "agent-text",
        serde_json::json!({ "text": "intermediate" }),
    )
    .await
    .unwrap();
    db.append_event("sub: scout", "user", serde_json::json!({ "text": "nudge" }))
        .await
        .unwrap();
    db.append_event(
        "sub: scout",
        "agent-text",
        serde_json::json!({ "text": "final findings" }),
    )
    .await
    .unwrap();

    let session = db.get_session("sub: scout").await.unwrap().unwrap();
    let (parent, text) = peckboard::subagent::claim_and_compose(&db, &session, true, None)
        .await
        .unwrap();
    assert_eq!(parent, "parent");
    assert!(text.contains("[subagent \"scout\""));
    assert!(text.contains("finished]"));
    // Only text after the LAST user event is the reply.
    assert!(text.contains("final findings"));
    assert!(!text.contains("intermediate"));

    // Idempotent: the second claim reports nothing.
    assert!(
        peckboard::subagent::claim_and_compose(&db, &session, true, None)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn claim_and_compose_crash_and_missing_parent() {
    let db = Db::in_memory().unwrap();
    seed_folder(&db, "f1").await;
    seed_session(&db, "parent", "f1", None).await;
    seed_session(&db, "sub: a", "f1", Some("parent")).await;
    seed_session(&db, "sub: b", "f1", Some("ghost-parent")).await;

    let a = db.get_session("sub: a").await.unwrap().unwrap();
    let (_, text) = peckboard::subagent::claim_and_compose(&db, &a, false, Some("boom"))
        .await
        .unwrap();
    assert!(text.contains("CRASHED"));
    assert!(text.contains("boom"));

    // Parent gone → nothing to deliver (but the claim is still consumed).
    let b = db.get_session("sub: b").await.unwrap().unwrap();
    assert!(
        peckboard::subagent::claim_and_compose(&db, &b, true, None)
            .await
            .is_none()
    );
    assert!(
        db.get_session("sub: b")
            .await
            .unwrap()
            .unwrap()
            .subagent_completed_at
            .is_some()
    );
}
