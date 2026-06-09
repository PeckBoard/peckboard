//! Integration test for the `ask_expert` MCP tool (asynchronous Q&A).
//!
//! Asserts the locked async contract end-to-end against the public registry +
//! an in-memory DB (no live agent / dispatcher):
//! - a caller's question lands as a `user` event on the target expert session,
//! - a context-coupled answer event is delivered back to the asking session,
//! - scope is enforced: a caller cannot reach an expert in another project,
//! - reply-mode delivers an expert's answer back to the asking session.

use std::sync::Arc;

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewProject, NewSession};
use peckboard::service::mcp_server::{McpToolRegistry, ToolCallContext};
use peckboard::ws::broadcaster::Broadcaster;

async fn seed_project(db: &Db, project_id: &str, folder_id: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: folder_id.into(),
        name: "F".into(),
        path: format!("/tmp/ask-expert/{folder_id}"),
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
        default_workflow: None,
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

#[allow(clippy::too_many_arguments)]
async fn seed_session(
    db: &Db,
    id: &str,
    folder_id: &str,
    project_id: Option<&str>,
    is_worker: bool,
    is_expert: bool,
    knowledge_area: Option<&str>,
    knowledge_summary: Option<&str>,
    scope_path: Option<&str>,
) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_session(NewSession {
        id: id.into(),
        name: format!("session {id}"),
        folder_id: folder_id.into(),
        model: Some("mock:happy-path".into()),
        effort: None,
        is_worker,
        project_id: project_id.map(|s| s.to_string()),
        card_id: None,
        conversation_id: None,
        created_at: ts.clone(),
        last_activity: ts.clone(),
        is_expert,
        expert_kind: is_expert.then(|| "knowledge".to_string()),
        knowledge_summary: knowledge_summary.map(|s| s.to_string()),
        knowledge_area: knowledge_area.map(|s| s.to_string()),
        scope_path: scope_path.map(|s| s.to_string()),
        is_permanent: false,
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
async fn ask_expert_delivers_question_and_answer() {
    let db = Arc::new(Db::in_memory().unwrap());
    seed_project(&db, "p1", "f1").await;
    seed_session(
        &db,
        "caller",
        "f1",
        Some("p1"),
        true,
        false,
        None,
        None,
        None,
    )
    .await;
    seed_session(
        &db,
        "expert-auth",
        "f1",
        Some("p1"),
        false,
        true,
        Some("authentication"),
        Some("Knowledge area: authentication\nHandles login, JWT, sessions."),
        Some("src/auth"),
    )
    .await;

    let registry = McpToolRegistry::new();
    let result = registry
        .handle_tool_call(
            "ask_expert",
            serde_json::json!({
                "expert_id": "expert-auth",
                "question": "How are JWTs validated?",
            }),
            &ctx(&db, "caller", Some("p1")),
        )
        .await
        .unwrap();

    assert_eq!(result["status"], "ok");
    assert_eq!(result["expert_id"], "expert-auth");

    // The question landed as an event on the expert session.
    let expert_events = event_texts(&db, "expert-auth").await;
    assert!(
        expert_events
            .iter()
            .any(|t| t.contains("How are JWTs validated?") && t.contains("consultation")),
        "expert must receive the question as an event, got: {expert_events:?}"
    );

    // An answer event, coupled with context, was delivered back to the caller.
    let caller_events = event_texts(&db, "caller").await;
    assert!(
        caller_events.iter().any(|t| {
            t.contains("Expert answer")
                && t.contains("How are JWTs validated?")
                && t.contains("authentication")
        }),
        "caller must receive a context-coupled answer, got: {caller_events:?}"
    );
}

#[tokio::test]
async fn ask_expert_resolves_target_by_area() {
    let db = Arc::new(Db::in_memory().unwrap());
    seed_project(&db, "p1", "f1").await;
    seed_session(
        &db,
        "caller",
        "f1",
        Some("p1"),
        true,
        false,
        None,
        None,
        None,
    )
    .await;
    seed_session(
        &db,
        "expert-billing",
        "f1",
        Some("p1"),
        false,
        true,
        Some("billing"),
        Some("Knowledge area: billing\nInvoices and payments."),
        Some("src/billing"),
    )
    .await;
    seed_session(
        &db,
        "expert-auth",
        "f1",
        Some("p1"),
        false,
        true,
        Some("authentication"),
        Some("Knowledge area: authentication"),
        Some("src/auth"),
    )
    .await;

    let registry = McpToolRegistry::new();
    let result = registry
        .handle_tool_call(
            "ask_expert",
            serde_json::json!({
                "area": "billing",
                "question": "How do refunds work?",
            }),
            &ctx(&db, "caller", Some("p1")),
        )
        .await
        .unwrap();

    assert_eq!(result["status"], "ok");
    assert_eq!(
        result["expert_id"], "expert-billing",
        "area hint must resolve to the billing expert"
    );
}

#[tokio::test]
async fn ask_expert_rejects_cross_project_target() {
    let db = Arc::new(Db::in_memory().unwrap());
    seed_project(&db, "p1", "f1").await;
    seed_project(&db, "p2", "f2").await;
    seed_session(
        &db,
        "caller",
        "f1",
        Some("p1"),
        true,
        false,
        None,
        None,
        None,
    )
    .await;
    // Expert lives in p2; caller is scoped to p1.
    seed_session(
        &db,
        "expert-p2",
        "f2",
        Some("p2"),
        false,
        true,
        Some("secrets"),
        Some("p2 internals"),
        Some("src"),
    )
    .await;

    let registry = McpToolRegistry::new();
    let err = registry
        .handle_tool_call(
            "ask_expert",
            serde_json::json!({
                "expert_id": "expert-p2",
                "question": "leak something",
            }),
            &ctx(&db, "caller", Some("p1")),
        )
        .await;
    assert!(err.is_err(), "cross-project expert target must be rejected");
}

#[tokio::test]
async fn ask_expert_allows_global_expert() {
    let db = Arc::new(Db::in_memory().unwrap());
    seed_project(&db, "p1", "f1").await;
    seed_session(
        &db,
        "caller",
        "f1",
        Some("p1"),
        true,
        false,
        None,
        None,
        None,
    )
    .await;
    // Global expert: project_id = NULL.
    seed_session(
        &db,
        "expert-global",
        "f1",
        None,
        false,
        true,
        Some("conventions"),
        Some("Repo-wide conventions."),
        Some("."),
    )
    .await;

    let registry = McpToolRegistry::new();
    let result = registry
        .handle_tool_call(
            "ask_expert",
            serde_json::json!({
                "expert_id": "expert-global",
                "question": "What is the commit style?",
            }),
            &ctx(&db, "caller", Some("p1")),
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "ok");
    assert_eq!(result["expert_id"], "expert-global");
}

#[tokio::test]
async fn ask_expert_reply_mode_routes_answer_to_caller() {
    let db = Arc::new(Db::in_memory().unwrap());
    seed_project(&db, "p1", "f1").await;
    seed_session(
        &db,
        "caller",
        "f1",
        Some("p1"),
        true,
        false,
        None,
        None,
        None,
    )
    .await;
    seed_session(
        &db,
        "expert-auth",
        "f1",
        Some("p1"),
        false,
        true,
        Some("authentication"),
        Some("auth knowledge"),
        Some("src/auth"),
    )
    .await;

    let registry = McpToolRegistry::new();
    // The expert replies back to the asking session.
    let result = registry
        .handle_tool_call(
            "ask_expert",
            serde_json::json!({
                "reply_to_session_id": "caller",
                "answer": "JWTs are validated against the signing key in middleware.",
                "question": "How are JWTs validated?",
            }),
            &ctx(&db, "expert-auth", Some("p1")),
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "ok");
    assert_eq!(result["reply_to_session_id"], "caller");

    let caller_events = event_texts(&db, "caller").await;
    assert!(
        caller_events.iter().any(|t| {
            t.contains("Expert answer")
                && t.contains("signing key in middleware")
                && t.contains("How are JWTs validated?")
        }),
        "caller must receive the expert's reply coupled with context, got: {caller_events:?}"
    );
}
