//! `spawn_subagent` must reject an unknown/typo'd/deleted-account `model`
//! immediately (no child row created) instead of creating one that can
//! never dispatch and permanently occupies a concurrency slot.

use std::sync::Arc;

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewSession};
use peckboard::provider::mock::register_mock_provider;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::service::mcp_server::{McpToolRegistry, ToolCallContext};
use peckboard::ws::broadcaster::Broadcaster;

async fn seed_folder(db: &Db, id: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: id.into(),
        name: id.into(),
        path: format!("/tmp/subagent-model-validation-test/{id}"),
        created_at: ts,
    })
    .await
    .unwrap();
}

async fn seed_session(db: &Db, id: &str, folder_id: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_session(NewSession {
        id: id.into(),
        name: id.into(),
        folder_id: folder_id.into(),
        model: Some("mock:echo".into()),
        created_at: ts.clone(),
        last_activity: ts,
        ..Default::default()
    })
    .await
    .unwrap();
}

async fn spawn(
    registry: &McpToolRegistry,
    ctx: &ToolCallContext,
    args: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    registry.handle_tool_call("spawn_subagent", args, ctx).await
}

#[tokio::test]
async fn unknown_explicit_model_is_rejected_before_creating_a_child_row() {
    let db = Arc::new(Db::in_memory().unwrap());
    seed_folder(&db, "f1").await;
    seed_session(&db, "parent", "f1").await;

    let provider_registry = Arc::new(ProviderRegistry::new());
    register_mock_provider(&provider_registry).await;

    let ctx = ToolCallContext {
        session_id: "parent".into(),
        project_id: None,
        card_id: None,
        folder_id: "f1".into(),
        db: db.clone(),
        broadcaster: Broadcaster::new(),
        provider_registry: Some(provider_registry),
        data_dir: None,
    };
    let registry = McpToolRegistry::new();

    let err = spawn(
        &registry,
        &ctx,
        serde_json::json!({
            "name": "scout",
            "prompt": "map the repo layout",
            "model": "mock:definitely-not-a-real-model",
        }),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("unknown model"));

    // No child row leaked a concurrency slot.
    assert_eq!(db.count_active_subagents("parent").await.unwrap(), 0);
}

#[tokio::test]
async fn known_model_still_spawns_normally() {
    let db = Arc::new(Db::in_memory().unwrap());
    seed_folder(&db, "f1").await;
    seed_session(&db, "parent", "f1").await;

    let provider_registry = Arc::new(ProviderRegistry::new());
    register_mock_provider(&provider_registry).await;

    let ctx = ToolCallContext {
        session_id: "parent".into(),
        project_id: None,
        card_id: None,
        folder_id: "f1".into(),
        db: db.clone(),
        broadcaster: Broadcaster::new(),
        provider_registry: Some(provider_registry),
        data_dir: None,
    };
    let registry = McpToolRegistry::new();

    let result = spawn(
        &registry,
        &ctx,
        serde_json::json!({
            "name": "scout",
            "prompt": "map the repo layout",
            "model": "mock:echo",
        }),
    )
    .await
    .unwrap();
    assert_eq!(result["status"], "ok");
    assert_eq!(db.count_active_subagents("parent").await.unwrap(), 1);
}

#[tokio::test]
async fn inherited_model_from_caller_is_also_validated() {
    let db = Arc::new(Db::in_memory().unwrap());
    seed_folder(&db, "f1").await;
    // Parent pinned to a model no registered provider serves anymore
    // (e.g. a deleted account) — spawn_subagent inherits it by default.
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_session(NewSession {
        id: "parent".into(),
        name: "parent".into(),
        folder_id: "f1".into(),
        model: Some("mock:@deleted-account".into()),
        created_at: ts.clone(),
        last_activity: ts,
        ..Default::default()
    })
    .await
    .unwrap();

    let provider_registry = Arc::new(ProviderRegistry::new());
    register_mock_provider(&provider_registry).await;

    let ctx = ToolCallContext {
        session_id: "parent".into(),
        project_id: None,
        card_id: None,
        folder_id: "f1".into(),
        db: db.clone(),
        broadcaster: Broadcaster::new(),
        provider_registry: Some(provider_registry),
        data_dir: None,
    };
    let registry = McpToolRegistry::new();

    let err = spawn(
        &registry,
        &ctx,
        serde_json::json!({ "name": "scout", "prompt": "map the repo layout" }),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("unknown model"));
    assert_eq!(db.count_active_subagents("parent").await.unwrap(), 0);
}
