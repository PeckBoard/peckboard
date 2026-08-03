//! MCP session-read ownership boundary.
//!
//! `read_worker_session` / `search_sessions` / `list_sessions` /
//! `set_session_system_prompt` scope by folder (and project, for worker
//! tokens) via `readable_sessions_in_scope` / `scope_readable_session` in
//! `src/service/mcp_server/handlers/workers.rs`. A plain chat session
//! (`project_id IS NULL`) is private to its owner at the HTTP/WS layer
//! (`auth::access::may_access_session`) — these tests confirm the MCP
//! layer now enforces the same rule instead of treating "same folder" as
//! sufficient.

use std::sync::Arc;

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewSession, NewUser};
use peckboard::service::mcp_server::{McpToolRegistry, ToolCallContext};
use peckboard::ws::broadcaster::Broadcaster;

async fn seed_folder(db: &Db, id: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: id.into(),
        name: id.into(),
        path: format!("/tmp/mcp-owner/{id}"),
        created_at: ts,
    })
    .await
    .unwrap();
}

async fn seed_user(db: &Db, id: &str, role: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_user(NewUser {
        id: id.into(),
        username: id.into(),
        email: None,
        password_hash: "h".into(),
        role: role.into(),
        created_at: ts.clone(),
        updated_at: ts,
    })
    .await
    .unwrap();
}

/// A plain chat session (no project) owned by `owner`.
async fn seed_chat_session(db: &Db, id: &str, folder_id: &str, owner: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_session(NewSession {
        id: id.into(),
        name: id.into(),
        folder_id: folder_id.into(),
        model: None,
        effort: None,
        is_worker: false,
        project_id: None,
        card_id: None,
        conversation_id: None,
        created_at: ts.clone(),
        last_activity: ts,
        is_expert: false,
        expert_kind: None,
        knowledge_summary: None,
        knowledge_area: None,
        scope_path: None,
        is_permanent: false,
        repeating_task_id: None,
        system_prompt: None,
        handover_run_id: None,
        handover_to_model: None,
        pending_handover_doc: None,
        worker_step: None,
        user_id: Some(owner.to_string()),
        model_autoswitch: None,
        context_reset_ts: None,
        system_prompt_name: None,
        is_temp: false,
        parent_session_id: None,
        subagent_completed_at: None,
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

/// Standard fixture: folder f1, member users a/b, admin user adm, and one
/// private chat session per user (chat-a, chat-b, chat-adm).
async fn fixture(db: &Db) {
    seed_folder(db, "f1").await;
    seed_user(db, "u-a", "member").await;
    seed_user(db, "u-b", "member").await;
    seed_user(db, "u-adm", "admin").await;
    seed_chat_session(db, "chat-a", "f1", "u-a").await;
    seed_chat_session(db, "chat-b", "f1", "u-b").await;
    seed_chat_session(db, "chat-adm", "f1", "u-adm").await;
}

#[tokio::test]
async fn read_worker_session_blocks_other_users_private_chat() {
    let db = Arc::new(Db::in_memory().unwrap());
    fixture(&db).await;
    db.append_event(
        "chat-a",
        "agent-text",
        serde_json::json!({ "text": "user A's secret" }),
    )
    .await
    .unwrap();
    let registry = McpToolRegistry::new();

    let err = registry
        .handle_tool_call(
            "read_worker_session",
            serde_json::json!({ "session_id": "chat-a" }),
            &ctx(&db, "chat-b", "f1"),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not found"), "got: {err}");
}

#[tokio::test]
async fn read_worker_session_allows_owner_across_their_own_sessions() {
    let db = Arc::new(Db::in_memory().unwrap());
    fixture(&db).await;
    seed_chat_session(&db, "chat-a2", "f1", "u-a").await;
    let registry = McpToolRegistry::new();

    let ok = registry
        .handle_tool_call(
            "read_worker_session",
            serde_json::json!({ "session_id": "chat-a2" }),
            &ctx(&db, "chat-a", "f1"),
        )
        .await
        .unwrap();
    assert_eq!(ok["session_id"], "chat-a2");
}

#[tokio::test]
async fn read_worker_session_allows_admin_to_read_any_users_chat() {
    let db = Arc::new(Db::in_memory().unwrap());
    fixture(&db).await;
    let registry = McpToolRegistry::new();

    let ok = registry
        .handle_tool_call(
            "read_worker_session",
            serde_json::json!({ "session_id": "chat-a" }),
            &ctx(&db, "chat-adm", "f1"),
        )
        .await
        .unwrap();
    assert_eq!(ok["session_id"], "chat-a");
}

#[tokio::test]
async fn search_sessions_targeted_blocks_other_users_private_chat() {
    let db = Arc::new(Db::in_memory().unwrap());
    fixture(&db).await;
    db.append_event(
        "chat-a",
        "agent-text",
        serde_json::json!({ "text": "password: hunter2" }),
    )
    .await
    .unwrap();
    let registry = McpToolRegistry::new();

    let err = registry
        .handle_tool_call(
            "search_sessions",
            serde_json::json!({ "session_id": "chat-a", "query": "password" }),
            &ctx(&db, "chat-b", "f1"),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not found"), "got: {err}");
}

#[tokio::test]
async fn search_sessions_folder_wide_omits_other_users_private_chats() {
    let db = Arc::new(Db::in_memory().unwrap());
    fixture(&db).await;
    for sid in ["chat-a", "chat-b"] {
        db.append_event(
            sid,
            "agent-text",
            serde_json::json!({ "text": "needle found" }),
        )
        .await
        .unwrap();
    }
    let registry = McpToolRegistry::new();

    let res = registry
        .handle_tool_call(
            "search_sessions",
            serde_json::json!({ "query": "needle" }),
            &ctx(&db, "chat-b", "f1"),
        )
        .await
        .unwrap();
    let sessions: Vec<&str> = res["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["session_id"].as_str().unwrap())
        .collect();
    assert!(sessions.contains(&"chat-b"), "must see own chat: {res}");
    assert!(
        !sessions.contains(&"chat-a"),
        "must not see another user's private chat: {res}"
    );
}

#[tokio::test]
async fn list_sessions_omits_other_users_private_chats() {
    let db = Arc::new(Db::in_memory().unwrap());
    fixture(&db).await;
    let registry = McpToolRegistry::new();

    let res = registry
        .handle_tool_call(
            "list_sessions",
            serde_json::json!({}),
            &ctx(&db, "chat-b", "f1"),
        )
        .await
        .unwrap();
    let ids: Vec<&str> = res["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["session_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"chat-b"));
    assert!(
        !ids.contains(&"chat-a"),
        "leaked another user's chat: {res}"
    );
}

#[tokio::test]
async fn set_session_system_prompt_blocks_other_users_private_chat() {
    let db = Arc::new(Db::in_memory().unwrap());
    fixture(&db).await;
    let registry = McpToolRegistry::new();

    let err = registry
        .handle_tool_call(
            "set_session_system_prompt",
            serde_json::json!({ "session_id": "chat-a", "system_prompt": "pwn" }),
            &ctx(&db, "chat-b", "f1"),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not found"), "got: {err}");
    let target = db.get_session("chat-a").await.unwrap().unwrap();
    assert_eq!(target.system_prompt, None);
}
