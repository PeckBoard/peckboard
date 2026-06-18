//! POST /api/sessions/:id/clear wipes a session's transcript and resets
//! it to a fresh state — useful for a plain chat the user wants to start
//! over with, harmful for sessions that are owned by another system.
//!
//! Two cases are guarded with 409:
//!
//! - **Worker sessions** — their transcript is the card's audit trail
//!   and the orchestrator needs it to resume / hand off. Clearing it
//!   from under the worker corrupts the card record.
//! - **Repeating-task sessions** — these are the run history of a
//!   schedule. Clearing one wipes the audit trail of a scheduled run
//!   without removing the row, leaving a confusing empty stub the
//!   schedule keeps firing past. Users delete instead of clearing
//!   ([[deleting-sessions]] card).
//!
//! Plain chat sessions still clear cleanly.
//!
//! Mirrors the structure of [`delete_worker_session`] so future readers
//! recognise the two-file guard pattern at a glance.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{
    NewAuthSession, NewCard, NewFolder, NewProject, NewRepeatingTask, NewSession, NewUser,
};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::routes::sessions::router;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::ws::broadcaster::Broadcaster;
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
        builtin_plugins: Arc::new(BuiltinPluginRegistry::new()),
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
    });

    std::mem::forget(tmp);

    (state, token)
}

async fn seed(state: &AppState) {
    let ts = chrono::Utc::now().to_rfc3339();
    state
        .db
        .create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
    state
        .db
        .create_project(NewProject {
            id: "p1".into(),
            name: "P".into(),
            context: "".into(),
            folder_id: "f1".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "task".into(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: true,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts.clone(),
        })
        .await
        .unwrap();
    state
        .db
        .create_card(NewCard {
            id: "c1".into(),
            project_id: "p1".into(),
            title: "Card".into(),
            description: "".into(),
            step: "in_progress".into(),
            priority: 1,
            workflow: "task".into(),
            model: None,
            effort: None,
            blocked: false,
            block_reason: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
        })
        .await
        .unwrap();
    state
        .db
        .create_repeating_task(NewRepeatingTask {
            id: "rt1".into(),
            name: "Daily standup".into(),
            description: "".into(),
            folder_id: "f1".into(),
            prompt: "summarise yesterday".into(),
            schedule_kind: "daily".into(),
            schedule_value: "\"09:00\"".into(),
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

    // A plain chat session — clearable.
    state
        .db
        .create_session(NewSession {
            id: "plain".into(),
            name: "Chat".into(),
            folder_id: "f1".into(),
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();

    // A worker session for the card — NOT clearable.
    state
        .db
        .create_session(NewSession {
            id: "worker".into(),
            name: "worker: Card".into(),
            folder_id: "f1".into(),
            is_worker: true,
            project_id: Some("p1".into()),
            card_id: Some("c1".into()),
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();

    // A session kicked off by a repeating task — NOT clearable.
    state
        .db
        .create_session(NewSession {
            id: "rt-run".into(),
            name: "Daily standup — run".into(),
            folder_id: "f1".into(),
            repeating_task_id: Some("rt1".into()),
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();

    // Seed each session with a stub event so the clear's
    // `delete_events_by_session` has something to remove and the assert
    // that events survive after a refused clear actually has teeth.
    for sid in ["plain", "worker", "rt-run"] {
        state
            .db
            .append_event(sid, "user", serde_json::json!({ "text": "hello" }))
            .await
            .unwrap();
    }
}

async fn clear_session(state: Arc<AppState>, token: &str, id: &str) -> StatusCode {
    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/sessions/{id}/clear"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = router(state.clone())
        .with_state(state)
        .oneshot(req)
        .await
        .unwrap();
    resp.status()
}

#[tokio::test]
async fn worker_session_clear_is_refused_and_events_survive() {
    let (state, token) = build_state().await;
    seed(&state).await;

    let status = clear_session(state.clone(), &token, "worker").await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "POST /clear on a worker session must be refused with 409",
    );

    let events = state
        .db
        .list_events_by_session("worker", None)
        .await
        .unwrap();
    assert!(
        !events.is_empty(),
        "worker session events must survive the rejected clear",
    );
}

#[tokio::test]
async fn repeating_task_session_clear_is_refused_and_events_survive() {
    let (state, token) = build_state().await;
    seed(&state).await;

    let status = clear_session(state.clone(), &token, "rt-run").await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "POST /clear on a repeating-task session must be refused with 409",
    );

    let events = state
        .db
        .list_events_by_session("rt-run", None)
        .await
        .unwrap();
    assert!(
        !events.is_empty(),
        "repeating-task session events must survive the rejected clear",
    );
}

#[tokio::test]
async fn plain_session_still_clears_cleanly() {
    let (state, token) = build_state().await;
    seed(&state).await;

    let status = clear_session(state.clone(), &token, "plain").await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "POST /clear on a plain chat session must still succeed",
    );

    let events = state
        .db
        .list_events_by_session("plain", None)
        .await
        .unwrap();
    assert!(events.is_empty(), "plain session events must be wiped");
}

#[tokio::test]
async fn clear_on_unknown_session_returns_404() {
    // Sanity: the 409 guards must not shadow the existing 404 for
    // a session id that doesn't exist at all.
    let (state, token) = build_state().await;
    seed(&state).await;

    let status = clear_session(state.clone(), &token, "ghost").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
