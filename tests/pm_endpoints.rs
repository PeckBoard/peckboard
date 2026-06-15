//! HTTP-level tests for the PM decision-log API surface
//! (`/api/projects/:id/pm/...`), driving the projects router directly via
//! `tower::ServiceExt::oneshot` against `Db::in_memory()`:
//! - list answered decisions (superseded excluded, pending_count attached,
//!   no cross-project leakage),
//! - list pending questions,
//! - answer a pending question (becomes a decision, the PM expert session
//!   receives the answer event, a supersession authorization is granted,
//!   the export regenerates, a `pm-decisions-changed` event broadcasts),
//! - answering twice conflicts (409),
//! - the user supersedes an answered decision via PUT,
//! - 404s for unknown / cross-project ids,
//! - all routes sit behind the auth middleware (401 without a token).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewAuthSession, NewFolder, NewProject, NewSession, NewUser};
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::routes::projects::router;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::pm_expert::{PM_DECISIONS_FILE, pm_decisions_folder, project_pm_expert_id};
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::ws::broadcaster::Broadcaster;
use serde_json::Value;
use tower::ServiceExt;

async fn build_state() -> (Arc<AppState>, String) {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        port: 0,
        https_port: 0,
        host: "127.0.0.1".into(),
        data_dir: tmp.path().to_path_buf(),
        mdns: false,
    };

    let db = Db::in_memory().unwrap();
    let plugins = Arc::new(PluginManager::new(&config.data_dir, db.clone()));
    let jwt_secret = generate_jwt_secret();
    let provider_registry = Arc::new(ProviderRegistry::new());
    let session_manager = SessionManager::new(provider_registry.clone());
    let push_service = PushService::new(&config.data_dir);

    let now_secs = 1_000_000i64;
    db.create_user(NewUser {
        id: "u1".into(),
        username: "admin".into(),
        email: None,
        password_hash: "h".into(),
        role: "admin".into(),
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
    })
    .await
    .unwrap();
    let (token, _exp) = create_token(&jwt_secret, "u1", "admin", "as1").unwrap();
    db.create_auth_session(NewAuthSession {
        id: "as1".into(),
        user_id: "u1".into(),
        token_hash: hash_token(&token),
        created_at: now_secs,
        expires_at: now_secs + 7 * 24 * 60 * 60,
        user_agent: None,
        ip_address: None,
    })
    .await
    .unwrap();

    let state = Arc::new(AppState {
        config,
        db,
        plugins,
        builtin_plugins: Arc::new(peckboard::plugin::builtin::BuiltinPluginRegistry::new()),
        jwt_secret,
        login_limiter: RateLimiter::new(60),
        password_change_limiter: RateLimiter::<String>::new(5),
        broadcaster: Broadcaster::new(),
        provider_registry,
        session_manager,
        repeating_task_manager: peckboard::repeating::RepeatingTaskManager::new(),
        run_auditor: peckboard::repeating::RunAuditor::new(),
        mcp_tokens: McpTokenRegistry::new(),
        push_service,
        pm_authorizations: Default::default(),
    });

    std::mem::forget(tmp);
    (state, token)
}

async fn seed_project(state: &AppState, project_id: &str, folder_id: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    state
        .db
        .create_folder(NewFolder {
            id: folder_id.into(),
            name: "F".into(),
            path: format!("/tmp/pm-endpoints/{folder_id}"),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
    state
        .db
        .create_project(NewProject {
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

async fn seed_worker(state: &AppState, id: &str, folder_id: &str, project_id: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    state
        .db
        .create_session(NewSession {
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

async fn send(
    state: Arc<AppState>,
    token: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let req = match body {
        Some(json) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&json).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let resp = router(state.clone())
        .with_state(state)
        .oneshot(req)
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

fn read_export(state: &AppState, project_id: &str) -> String {
    std::fs::read_to_string(
        state
            .config
            .data_dir
            .join("reports")
            .join(pm_decisions_folder(project_id))
            .join(PM_DECISIONS_FILE),
    )
    .unwrap()
}

#[tokio::test]
async fn list_decisions_returns_answered_only_with_pending_count() {
    let (state, token) = build_state().await;
    seed_project(&state, "p1", "f1").await;
    seed_project(&state, "p2", "f2").await;

    let kept = state
        .db
        .record_decision("p1", "Currency handling", "Integer cents.", None)
        .await
        .unwrap();
    let old = state
        .db
        .record_decision("p1", "Auth provider", "Roll our own.", None)
        .await
        .unwrap();
    state
        .db
        .supersede_decision(&old.id, "Auth provider", "Use OAuth2.")
        .await
        .unwrap();
    state
        .db
        .create_pending_question("p1", "Ship the beta this week?", None)
        .await
        .unwrap();
    // Another project's decision must not leak into p1's list.
    state
        .db
        .record_decision("p2", "Other project", "Other answer.", None)
        .await
        .unwrap();

    let (status, body) = send(
        state.clone(),
        Some(&token),
        "GET",
        "/api/projects/p1/pm/decisions",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["pending_count"], 1);

    let decisions = body["decisions"].as_array().unwrap();
    assert_eq!(decisions.len(), 2, "answered only, got: {decisions:?}");
    let questions: Vec<&str> = decisions
        .iter()
        .map(|d| d["question"].as_str().unwrap())
        .collect();
    assert!(questions.contains(&"Currency handling"));
    assert!(questions.contains(&"Auth provider"));
    let auth = decisions
        .iter()
        .find(|d| d["question"] == "Auth provider")
        .unwrap();
    assert_eq!(
        auth["answer"], "Use OAuth2.",
        "the superseding row must carry the live answer"
    );
    let kept_row = decisions
        .iter()
        .find(|d| d["id"] == kept.id.as_str())
        .unwrap();
    assert_eq!(kept_row["status"], "answered");
    assert!(kept_row["decided_at"].is_string());
}

#[tokio::test]
async fn list_questions_returns_pending_with_provenance() {
    let (state, token) = build_state().await;
    seed_project(&state, "p1", "f1").await;
    seed_worker(&state, "worker-1", "f1", "p1").await;

    let pending = state
        .db
        .create_pending_question("p1", "Integer cents?", Some("worker-1"))
        .await
        .unwrap();
    state
        .db
        .record_decision("p1", "Already decided", "Yes.", None)
        .await
        .unwrap();

    let (status, body) = send(
        state.clone(),
        Some(&token),
        "GET",
        "/api/projects/p1/pm/questions",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let questions = body["questions"].as_array().unwrap();
    assert_eq!(questions.len(), 1, "pending only, got: {questions:?}");
    assert_eq!(questions[0]["id"], pending.id.as_str());
    assert_eq!(questions[0]["question"], "Integer cents?");
    assert_eq!(questions[0]["asked_by_session_id"], "worker-1");
    assert!(questions[0]["asked_at"].is_string());
}

#[tokio::test]
async fn answer_pending_question_becomes_decision_and_feeds_pm_expert() {
    let (state, token) = build_state().await;
    seed_project(&state, "p1", "f1").await;
    seed_worker(&state, "worker-1", "f1", "p1").await;
    let pending = state
        .db
        .create_pending_question("p1", "Integer cents?", Some("worker-1"))
        .await
        .unwrap();

    let mut rx = state.broadcaster.subscribe_all();
    let (status, body) = send(
        state.clone(),
        Some(&token),
        "POST",
        &format!("/api/projects/p1/pm/questions/{}/answer", pending.id),
        Some(serde_json::json!({ "answer": "Yes — integer cents everywhere." })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "got: {body}");
    assert_eq!(body["decision"]["status"], "answered");
    assert_eq!(
        body["decision"]["answer"],
        "Yes — integer cents everywhere."
    );
    assert_eq!(body["pending_count"], 0);

    // The question row is now an answered decision.
    let active = state.db.list_answered_pm_decisions("p1").await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, pending.id);

    // The PM expert session received the answer as an express user decision.
    let pm_id = project_pm_expert_id("p1");
    let texts: Vec<String> = state
        .db
        .events_tail(&pm_id, 100)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| {
            serde_json::from_str::<Value>(&e.data)
                .ok()
                .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(String::from))
        })
        .collect();
    assert!(
        texts.iter().any(|t| t.contains("express user decision")
            && t.contains("Integer cents?")
            && t.contains("Yes — integer cents everywhere.")),
        "PM expert must receive the answer event, got: {texts:?}"
    );

    // The export side-effect ran and a one-shot supersession was authorized.
    let export = read_export(&state, "p1");
    assert!(
        export.contains("Yes — integer cents everywhere."),
        "export must reflect the answer, got: {export}"
    );
    assert!(
        state.pm_authorizations.consume("p1"),
        "answering must grant exactly one supersession authorization"
    );
    assert!(!state.pm_authorizations.consume("p1"));

    // The frontend got a live-update broadcast for the project.
    let mut saw_pm_change = false;
    while let Ok(ev) = rx.try_recv() {
        if ev.event_type == "pm-decisions-changed" && ev.session_id == "p1" {
            assert_eq!(ev.data["pending_count"], 0);
            saw_pm_change = true;
        }
    }
    assert!(
        saw_pm_change,
        "mutation must broadcast pm-decisions-changed"
    );
}

#[tokio::test]
async fn answering_twice_conflicts() {
    let (state, token) = build_state().await;
    seed_project(&state, "p1", "f1").await;
    let pending = state
        .db
        .create_pending_question("p1", "Integer cents?", None)
        .await
        .unwrap();

    let uri = format!("/api/projects/p1/pm/questions/{}/answer", pending.id);
    let (status, _) = send(
        state.clone(),
        Some(&token),
        "POST",
        &uri,
        Some(serde_json::json!({ "answer": "Yes." })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = send(
        state.clone(),
        Some(&token),
        "POST",
        &uri,
        Some(serde_json::json!({ "answer": "Actually no." })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "got: {body}");
    assert!(body["error"].as_str().unwrap().contains("not pending"));

    // The first answer stands untouched.
    let row = state
        .db
        .get_pm_decision(&pending.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.answer.as_deref(), Some("Yes."));
}

#[tokio::test]
async fn user_supersedes_decision_via_put() {
    let (state, token) = build_state().await;
    seed_project(&state, "p1", "f1").await;
    let old = state
        .db
        .record_decision("p1", "Currency handling", "Floats are fine.", None)
        .await
        .unwrap();

    let mut rx = state.broadcaster.subscribe_all();
    let (status, body) = send(
        state.clone(),
        Some(&token),
        "PUT",
        &format!("/api/projects/p1/pm/decisions/{}", old.id),
        Some(serde_json::json!({ "answer": "Integer cents only." })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "got: {body}");
    assert_eq!(body["superseded_decision_id"], old.id.as_str());
    assert_eq!(body["decision"]["question"], "Currency handling");
    assert_eq!(body["decision"]["answer"], "Integer cents only.");

    let old_row = state.db.get_pm_decision(&old.id).await.unwrap().unwrap();
    assert_eq!(old_row.status, "superseded");
    let active = state.db.list_answered_pm_decisions("p1").await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].answer.as_deref(), Some("Integer cents only."));

    let export = read_export(&state, "p1");
    assert!(
        export.contains("Integer cents only.") && export.contains("Superseded Decisions"),
        "export must reflect the supersession, got: {export}"
    );

    let mut saw_pm_change = false;
    while let Ok(ev) = rx.try_recv() {
        if ev.event_type == "pm-decisions-changed" && ev.session_id == "p1" {
            saw_pm_change = true;
        }
    }
    assert!(
        saw_pm_change,
        "supersession must broadcast pm-decisions-changed"
    );

    // A pending question cannot be edited as a decision — it must go
    // through the answer endpoint.
    let pending = state
        .db
        .create_pending_question("p1", "Still open?", None)
        .await
        .unwrap();
    let (status, _) = send(
        state.clone(),
        Some(&token),
        "PUT",
        &format!("/api/projects/p1/pm/decisions/{}", pending.id),
        Some(serde_json::json!({ "answer": "Edit attempt." })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn unknown_and_cross_project_ids_return_404() {
    let (state, token) = build_state().await;
    seed_project(&state, "p1", "f1").await;
    seed_project(&state, "p2", "f2").await;
    let other = state
        .db
        .create_pending_question("p2", "Other project question", None)
        .await
        .unwrap();
    let other_decision = state
        .db
        .record_decision("p2", "Other project decision", "Answer.", None)
        .await
        .unwrap();

    for uri in [
        "/api/projects/nope/pm/decisions".to_string(),
        "/api/projects/nope/pm/questions".to_string(),
    ] {
        let (status, _) = send(state.clone(), Some(&token), "GET", &uri, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "GET {uri}");
    }

    // Unknown question id, and a question that belongs to another project.
    for qid in ["missing", other.id.as_str()] {
        let (status, _) = send(
            state.clone(),
            Some(&token),
            "POST",
            &format!("/api/projects/p1/pm/questions/{qid}/answer"),
            Some(serde_json::json!({ "answer": "A." })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "answer {qid}");
    }
    // The cross-project row was not touched.
    let row = state.db.get_pm_decision(&other.id).await.unwrap().unwrap();
    assert_eq!(row.status, "pending");

    // Unknown decision id, and a decision that belongs to another project.
    for did in ["missing", other_decision.id.as_str()] {
        let (status, _) = send(
            state.clone(),
            Some(&token),
            "PUT",
            &format!("/api/projects/p1/pm/decisions/{did}"),
            Some(serde_json::json!({ "answer": "A." })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "supersede {did}");
    }
}

#[tokio::test]
async fn pm_routes_require_auth() {
    let (state, _token) = build_state().await;
    seed_project(&state, "p1", "f1").await;

    let (status, _) = send(
        state.clone(),
        None,
        "GET",
        "/api/projects/p1/pm/decisions",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
