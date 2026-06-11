//! Integration tests for the `pm_record_decision` / `pm_check_decisions`
//! MCP tools, against the public registry + an in-memory DB (no live
//! agent / dispatcher):
//! - a worker records a decision with its session id as provenance, and the
//!   PM expert is notified as a user-style event,
//! - a worker's supersede attempt is rejected (ADD-only for workers),
//! - pm_check_decisions returns active decisions, excludes superseded ones,
//!   and carries the ask-the-PM-expert instruction,
//! - the keyword filter narrows but never hides everything,
//! - project resolution: token scope is authoritative (a conflicting explicit
//!   project_id is rejected); an unscoped caller (e.g. a plain chat session)
//!   may pass an explicit project_id that must name an existing project.

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
        path: format!("/tmp/pm-mcp/{folder_id}"),
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

/// A plain chat session: not a worker, bound to no project.
async fn seed_chat_session(db: &Db, id: &str, folder_id: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_session(NewSession {
        id: id.into(),
        name: format!("session {id}"),
        folder_id: folder_id.into(),
        model: Some("mock:happy-path".into()),
        is_worker: false,
        project_id: None,
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
async fn pm_record_decision_stores_provenance_and_notifies_pm_expert() {
    let db = Arc::new(Db::in_memory().unwrap());
    seed_project(&db, "p1", "f1").await;
    seed_worker(&db, "worker-1", "f1", "p1").await;

    let registry = McpToolRegistry::new();
    let result = registry
        .handle_tool_call(
            "pm_record_decision",
            serde_json::json!({
                "title": "Currency handling",
                "decision": "All prices are stored in integer cents.",
                "rationale": "Avoids float rounding bugs.",
            }),
            &ctx(&db, "worker-1", Some("p1")),
        )
        .await
        .unwrap();

    assert_eq!(result["status"], "ok");
    assert_eq!(result["decision"]["title"], "Currency handling");
    let id = result["decision"]["id"].as_str().unwrap();
    assert!(!id.is_empty());

    // Stored with the calling session id as provenance.
    let stored = db.list_answered_pm_decisions("p1").await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].asked_by_session_id.as_deref(), Some("worker-1"));
    assert_eq!(stored[0].question, "Currency handling");
    let answer = stored[0].answer.as_deref().unwrap();
    assert!(
        answer.contains("integer cents") && answer.contains("Avoids float rounding bugs"),
        "decision text and rationale must both be stored, got: {answer}"
    );

    // The PM expert was notified (lazily ensured under its stable id).
    let pm_id = project_pm_expert_id("p1");
    let pm_events = event_texts(&db, &pm_id).await;
    assert!(
        pm_events.iter().any(|t| t.contains("PM decision recorded")
            && t.contains("Currency handling")
            && t.contains("worker-1")),
        "PM expert must receive a notification event, got: {pm_events:?}"
    );
}

#[tokio::test]
async fn pm_record_decision_rejects_worker_supersede_attempt() {
    let db = Arc::new(Db::in_memory().unwrap());
    seed_project(&db, "p1", "f1").await;
    seed_worker(&db, "worker-1", "f1", "p1").await;
    let existing = db
        .record_decision("p1", "Currency handling", "Integer cents.", None)
        .await
        .unwrap();

    let registry = McpToolRegistry::new();
    let err = registry
        .handle_tool_call(
            "pm_record_decision",
            serde_json::json!({
                "title": "Currency handling v2",
                "decision": "Use floats actually.",
                "supersedes_decision_id": existing.id,
            }),
            &ctx(&db, "worker-1", Some("p1")),
        )
        .await;

    let msg = err.unwrap_err().to_string();
    assert!(
        msg.contains("PM expert"),
        "rejection must direct the worker to the PM expert, got: {msg}"
    );

    // Nothing was recorded and the existing decision is untouched.
    let stored = db.list_answered_pm_decisions("p1").await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].id, existing.id);
}

#[tokio::test]
async fn pm_record_decision_rejects_supersede_even_from_pm_expert() {
    // The user-authorization flag is not plumbed yet, so even the PM expert
    // itself cannot supersede via this tool.
    let db = Arc::new(Db::in_memory().unwrap());
    seed_project(&db, "p1", "f1").await;
    let existing = db
        .record_decision("p1", "Currency handling", "Integer cents.", None)
        .await
        .unwrap();

    let pm_id = project_pm_expert_id("p1");
    let registry = McpToolRegistry::new();
    let err = registry
        .handle_tool_call(
            "pm_record_decision",
            serde_json::json!({
                "title": "Currency handling v2",
                "decision": "Use floats actually.",
                "supersedes_decision_id": existing.id,
            }),
            &ctx(&db, &pm_id, Some("p1")),
        )
        .await;
    assert!(
        err.is_err(),
        "supersession must be rejected until user authorization is plumbed in"
    );
}

#[tokio::test]
async fn pm_check_decisions_returns_active_and_excludes_superseded() {
    let db = Arc::new(Db::in_memory().unwrap());
    seed_project(&db, "p1", "f1").await;
    seed_worker(&db, "worker-1", "f1", "p1").await;

    let old = db
        .record_decision("p1", "Currency handling", "Floats are fine.", None)
        .await
        .unwrap();
    let replacement = db
        .supersede_decision(&old.id, "Currency handling", "Integer cents only.")
        .await
        .unwrap();
    let other = db
        .record_decision("p1", "Auth provider", "OAuth2 via GitHub only.", None)
        .await
        .unwrap();
    // Pending questions are not decisions and must not appear either.
    db.create_pending_question("p1", "Mobile support?", None)
        .await
        .unwrap();

    let registry = McpToolRegistry::new();
    let result = registry
        .handle_tool_call(
            "pm_check_decisions",
            serde_json::json!({ "planned_change": "Refactor the payments module" }),
            &ctx(&db, "worker-1", Some("p1")),
        )
        .await
        .unwrap();

    assert_eq!(result["status"], "ok");
    assert_eq!(result["count"], 2);
    let ids: Vec<&str> = result["decisions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&replacement.id.as_str()), "got: {ids:?}");
    assert!(ids.contains(&other.id.as_str()), "got: {ids:?}");
    assert!(
        !ids.contains(&old.id.as_str()),
        "superseded decision must be excluded, got: {ids:?}"
    );

    // The response instructs the caller to escalate ambiguity to the PM expert.
    let instruction = result["instruction"].as_str().unwrap();
    assert!(
        instruction.contains("ask the PM expert via ask_expert before proceeding"),
        "got: {instruction}"
    );
}

#[tokio::test]
async fn pm_check_decisions_keyword_filter_narrows_but_never_empties() {
    let db = Arc::new(Db::in_memory().unwrap());
    seed_project(&db, "p1", "f1").await;
    seed_worker(&db, "worker-1", "f1", "p1").await;
    db.record_decision("p1", "Currency handling", "Integer cents only.", None)
        .await
        .unwrap();
    db.record_decision("p1", "Auth provider", "OAuth2 via GitHub only.", None)
        .await
        .unwrap();

    let registry = McpToolRegistry::new();

    // A matching keyword narrows the result.
    let narrowed = registry
        .handle_tool_call(
            "pm_check_decisions",
            serde_json::json!({
                "planned_change": "Switch login to email magic links",
                "topic_keywords": ["auth"],
            }),
            &ctx(&db, "worker-1", Some("p1")),
        )
        .await
        .unwrap();
    assert_eq!(narrowed["count"], 1);
    assert_eq!(narrowed["decisions"][0]["title"], "Auth provider");

    // A keyword that matches nothing falls back to the full active set.
    let fallback = registry
        .handle_tool_call(
            "pm_check_decisions",
            serde_json::json!({
                "planned_change": "Switch login to email magic links",
                "topic_keywords": ["zeppelin"],
            }),
            &ctx(&db, "worker-1", Some("p1")),
        )
        .await
        .unwrap();
    assert_eq!(
        fallback["count"], 2,
        "an unmatched keyword must never hide decisions, got: {fallback}"
    );
}

#[tokio::test]
async fn pm_check_decisions_accepts_explicit_project_id_from_unscoped_session() {
    // A plain chat session has no project context; an explicit project_id
    // input is honoured as a fallback so it can consult the decision log.
    let db = Arc::new(Db::in_memory().unwrap());
    seed_project(&db, "p1", "f1").await;
    seed_chat_session(&db, "chat-1", "f1").await;
    let decision = db
        .record_decision("p1", "Currency handling", "Integer cents only.", None)
        .await
        .unwrap();

    let registry = McpToolRegistry::new();
    let result = registry
        .handle_tool_call(
            "pm_check_decisions",
            serde_json::json!({
                "planned_change": "Refactor the payments module",
                "project_id": "p1",
            }),
            &ctx(&db, "chat-1", None),
        )
        .await
        .unwrap();

    assert_eq!(result["status"], "ok");
    assert_eq!(result["count"], 1);
    assert_eq!(result["decisions"][0]["id"], decision.id.as_str());
}

#[tokio::test]
async fn pm_record_decision_accepts_explicit_project_id_from_unscoped_session() {
    let db = Arc::new(Db::in_memory().unwrap());
    seed_project(&db, "p1", "f1").await;
    seed_chat_session(&db, "chat-1", "f1").await;

    let registry = McpToolRegistry::new();
    let result = registry
        .handle_tool_call(
            "pm_record_decision",
            serde_json::json!({
                "project_id": "p1",
                "title": "Currency handling",
                "decision": "Integer cents only.",
            }),
            &ctx(&db, "chat-1", None),
        )
        .await
        .unwrap();

    assert_eq!(result["status"], "ok");
    let stored = db.list_answered_pm_decisions("p1").await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].asked_by_session_id.as_deref(), Some("chat-1"));
}

#[tokio::test]
async fn pm_tools_reject_explicit_project_id_conflicting_with_token_scope() {
    // Token scope is authoritative: a caller scoped to p1 cannot redirect a
    // PM tool to p2 via the input.
    let db = Arc::new(Db::in_memory().unwrap());
    seed_project(&db, "p1", "f1").await;
    seed_project(&db, "p2", "f2").await;
    seed_worker(&db, "worker-1", "f1", "p1").await;

    let registry = McpToolRegistry::new();
    let err = registry
        .handle_tool_call(
            "pm_check_decisions",
            serde_json::json!({
                "planned_change": "anything",
                "project_id": "p2",
            }),
            &ctx(&db, "worker-1", Some("p1")),
        )
        .await;
    let msg = err.unwrap_err().to_string();
    // Conflict rejection uses "not found" framing for the foreign target
    // (no existence leak), but must still identify which id was bad.
    assert!(
        msg.contains("p2") && msg.contains("not found"),
        "conflict rejection must mark p2 as not found, got: {msg}"
    );

    let err = registry
        .handle_tool_call(
            "pm_record_decision",
            serde_json::json!({
                "project_id": "p2",
                "title": "Currency handling",
                "decision": "Integer cents only.",
            }),
            &ctx(&db, "worker-1", Some("p1")),
        )
        .await;
    assert!(err.is_err(), "scoped caller must not record into p2");
    assert!(
        db.list_answered_pm_decisions("p2")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn pm_tools_reject_unknown_explicit_project_id() {
    let db = Arc::new(Db::in_memory().unwrap());
    seed_project(&db, "p1", "f1").await;
    seed_chat_session(&db, "chat-1", "f1").await;

    let registry = McpToolRegistry::new();
    let err = registry
        .handle_tool_call(
            "pm_check_decisions",
            serde_json::json!({
                "planned_change": "anything",
                "project_id": "no-such-project",
            }),
            &ctx(&db, "chat-1", None),
        )
        .await;
    let msg = err.unwrap_err().to_string();
    assert!(msg.contains("not found"), "got: {msg}");
}

#[tokio::test]
async fn pm_tools_reject_caller_with_neither_scope_nor_explicit_project_id() {
    // No token scope and no explicit project_id: still rejected outright.
    let db = Arc::new(Db::in_memory().unwrap());
    seed_project(&db, "p1", "f1").await;
    seed_chat_session(&db, "chat-1", "f1").await;

    let registry = McpToolRegistry::new();
    let err = registry
        .handle_tool_call(
            "pm_check_decisions",
            serde_json::json!({ "planned_change": "anything" }),
            &ctx(&db, "chat-1", None),
        )
        .await;
    let msg = err.unwrap_err().to_string();
    assert!(msg.contains("project_id required"), "got: {msg}");
}
