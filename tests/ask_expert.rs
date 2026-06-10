//! Integration test for the `ask_expert` MCP tool (asynchronous Q&A).
//!
//! Asserts the locked async contract end-to-end against the public registry +
//! an in-memory DB (no live agent / dispatcher):
//! - a caller's question lands as a `user` event on the target expert session,
//! - a context-coupled answer event is delivered back to the asking session,
//! - scope is enforced: a caller cannot reach an expert in another project,
//! - reply-mode delivers an expert's answer back to the asking session.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewProject, NewSession};
use peckboard::service::mcp_server::{ExpertDispatcher, McpToolRegistry, ToolCallContext};
use peckboard::ws::broadcaster::Broadcaster;

/// Test dispatcher that records every `resume_session` call so a test can
/// assert which sessions got resumed (the production impl would call
/// `send_or_queue`). `dispatch_capture` is a no-op here.
#[derive(Default)]
struct RecordingDispatcher {
    resumed: Mutex<Vec<(String, String)>>,
}

impl ExpertDispatcher for RecordingDispatcher {
    fn dispatch_capture<'a>(
        &'a self,
        _expert_session_id: &'a str,
        _prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn resume_session<'a>(
        &'a self,
        session_id: &'a str,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        let entry = (session_id.to_string(), text.to_string());
        Box::pin(async move {
            self.resumed.lock().unwrap().push(entry);
            Ok(())
        })
    }
}

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
        repeating_task_id: None,
    })
    .await
    .unwrap();
}

/// Seed a project-scoped *question* expert (no codebase boundary).
async fn seed_question_expert(db: &Db, id: &str, folder_id: &str, project_id: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_session(NewSession {
        id: id.into(),
        name: format!("question {id}"),
        folder_id: folder_id.into(),
        model: Some("mock:happy-path".into()),
        effort: None,
        is_worker: false,
        project_id: Some(project_id.into()),
        card_id: None,
        conversation_id: None,
        created_at: ts.clone(),
        last_activity: ts.clone(),
        is_expert: true,
        expert_kind: Some("question".into()),
        knowledge_summary: Some("Accumulated user Q&A.".into()),
        knowledge_area: Some("User Q&A (project)".into()),
        scope_path: None,
        is_permanent: true,
        repeating_task_id: None,
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
async fn ask_expert_resumes_expert_and_caller_via_dispatcher() {
    // With a live dispatcher present, ask-mode resumes BOTH the target expert
    // (so an idle expert actually takes a turn to answer) AND the asking
    // session (so it processes the returned answer) — exactly like a user
    // message would.
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

    let recorder = Arc::new(RecordingDispatcher::default());
    let dispatcher: Arc<dyn ExpertDispatcher> = recorder.clone();
    let mut call_ctx = ctx(&db, "caller", Some("p1"));
    call_ctx.expert_dispatcher = Some(dispatcher);

    let registry = McpToolRegistry::new();
    registry
        .handle_tool_call(
            "ask_expert",
            serde_json::json!({
                "expert_id": "expert-auth",
                "question": "How are JWTs validated?",
            }),
            &call_ctx,
        )
        .await
        .unwrap();

    let resumed = recorder.resumed.lock().unwrap();
    assert!(
        resumed.iter().any(|(sid, _)| sid == "expert-auth"),
        "the target expert must be resumed, got: {resumed:?}"
    );
    assert!(
        resumed.iter().any(|(sid, _)| sid == "caller"),
        "the asking session must be resumed, got: {resumed:?}"
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
async fn ask_expert_rejects_cross_project_question_expert() {
    // A *question* expert holds private accumulated user Q&A and has no
    // codebase boundary, so it must stay project-scoped: a caller in p1
    // cannot reach p2's question expert.
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
    seed_question_expert(&db, "question-p2", "f2", "p2").await;

    let registry = McpToolRegistry::new();
    let err = registry
        .handle_tool_call(
            "ask_expert",
            serde_json::json!({
                "expert_id": "question-p2",
                "question": "leak something",
            }),
            &ctx(&db, "caller", Some("p1")),
        )
        .await;
    assert!(
        err.is_err(),
        "cross-project question expert must be rejected"
    );
}

#[tokio::test]
async fn ask_expert_cross_project_knowledge_expert_chat_only() {
    // A *knowledge* expert only answers within its codebase boundary. A chat
    // session (unscoped token) may consult it cross-project; a worker (a
    // project-scoped token) may not — workers stay confined to their project.
    let db = Arc::new(Db::in_memory().unwrap());
    seed_project(&db, "p1", "f1").await;
    seed_project(&db, "p2", "f2").await;
    // A worker in p1 and a plain chat session (no project).
    seed_session(
        &db,
        "worker-p1",
        "f1",
        Some("p1"),
        true,
        false,
        None,
        None,
        None,
    )
    .await;
    seed_session(&db, "chat", "f1", None, false, false, None, None, None).await;
    // Knowledge expert lives in p2.
    seed_session(
        &db,
        "expert-p2-web",
        "f2",
        Some("p2"),
        false,
        true,
        Some("web"),
        Some("Knowledge area: web\nReact frontend."),
        Some("web"),
    )
    .await;

    let registry = McpToolRegistry::new();

    // Worker scoped to p1 → rejected.
    let worker_err = registry
        .handle_tool_call(
            "ask_expert",
            serde_json::json!({
                "expert_id": "expert-p2-web",
                "question": "What guards the login form?",
            }),
            &ctx(&db, "worker-p1", Some("p1")),
        )
        .await;
    assert!(
        worker_err.is_err(),
        "a worker must not reach a cross-project expert"
    );

    // Chat session (unscoped) → allowed.
    let result = registry
        .handle_tool_call(
            "ask_expert",
            serde_json::json!({
                "expert_id": "expert-p2-web",
                "question": "What guards the login form?",
            }),
            &ctx(&db, "chat", None),
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "ok");
    assert_eq!(result["expert_id"], "expert-p2-web");
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

#[tokio::test]
async fn ask_expert_reply_mode_knowledge_expert_crosses_to_chat_only() {
    // A knowledge expert in p2 may reply to a cross-project *chat* session,
    // but not to a worker in another project.
    let db = Arc::new(Db::in_memory().unwrap());
    seed_project(&db, "p1", "f1").await;
    seed_project(&db, "p2", "f2").await;
    // Chat session (no project) and a worker in p1.
    seed_session(&db, "chat", "f1", None, false, false, None, None, None).await;
    seed_session(
        &db,
        "worker-p1",
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
        "expert-p2-web",
        "f2",
        Some("p2"),
        false,
        true,
        Some("web"),
        Some("web knowledge"),
        Some("web"),
    )
    .await;

    let registry = McpToolRegistry::new();

    // Reply to the chat session → delivered.
    let result = registry
        .handle_tool_call(
            "ask_expert",
            serde_json::json!({
                "reply_to_session_id": "chat",
                "answer": "The login form is guarded by a CSRF token in the auth store.",
                "question": "What guards the login form?",
            }),
            &ctx(&db, "expert-p2-web", Some("p2")),
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "ok");
    let chat_events = event_texts(&db, "chat").await;
    assert!(
        chat_events
            .iter()
            .any(|t| t.contains("Expert answer") && t.contains("CSRF token in the auth store")),
        "cross-project knowledge-expert reply must reach the chat session, got: {chat_events:?}"
    );

    // Reply to a worker in another project → rejected.
    let worker_err = registry
        .handle_tool_call(
            "ask_expert",
            serde_json::json!({
                "reply_to_session_id": "worker-p1",
                "answer": "should not be delivered",
                "question": "anything",
            }),
            &ctx(&db, "expert-p2-web", Some("p2")),
        )
        .await;
    assert!(
        worker_err.is_err(),
        "a knowledge expert must not push answers into a cross-project worker"
    );
}

#[tokio::test]
async fn ask_expert_reply_mode_question_expert_blocked_cross_project() {
    // A project-scoped question expert may NOT push an answer into a session
    // in another project.
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
    seed_question_expert(&db, "question-p2", "f2", "p2").await;

    let registry = McpToolRegistry::new();
    let err = registry
        .handle_tool_call(
            "ask_expert",
            serde_json::json!({
                "reply_to_session_id": "caller",
                "answer": "private p2 user answer",
                "question": "anything",
            }),
            &ctx(&db, "question-p2", Some("p2")),
        )
        .await;
    assert!(
        err.is_err(),
        "cross-project question-expert reply must be rejected"
    );
}
