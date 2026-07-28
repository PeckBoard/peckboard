//! Regression tests for validation messages that must name the right thing.
//!
//! Two defects motivated these:
//!
//! 1. `POST /api/users` replied with one static string — "username required,
//!    password min 12 chars" — for any failure, so a user who filled in the
//!    username correctly was told it was missing.
//! 2. `POST /api/repeating-tasks` with `{"minutes": 0}` must be rejected, not
//!    silently turned into "every 1 minute". The frontend used to clamp it.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewAuthSession, NewUser};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::routes::auth::router as auth_router;
use peckboard::routes::repeating_tasks::router as repeating_tasks_router;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::ws::broadcaster::Broadcaster;
use serde_json::Value;
use tower::ServiceExt;

struct Fixture {
    state: Arc<AppState>,
    admin_token: String,
}

async fn build_fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        port: 0,
        https_port: 0,
        host: "127.0.0.1".into(),
        data_dir: tmp.path().to_path_buf(),
        mdns: false,
        keep_alive_hours: 0,
        provider_send_timeout_secs: 300,
    };
    let db = Db::in_memory().unwrap();
    let plugins = Arc::new(PluginManager::new(&config.data_dir, db.clone()));
    let jwt_secret = generate_jwt_secret();
    let provider_registry = Arc::new(ProviderRegistry::new());
    let session_manager = SessionManager::new(provider_registry.clone());
    let push_service = PushService::new(&config.data_dir);

    let now = chrono::Utc::now().to_rfc3339();
    db.create_user(NewUser {
        id: "u1".into(),
        username: "admin".into(),
        email: None,
        password_hash: "h".into(),
        role: "admin".into(),
        created_at: now.clone(),
        updated_at: now,
    })
    .await
    .unwrap();
    let (admin_token, _exp) = create_token(&jwt_secret, "u1", "admin", "as1").unwrap();
    db.create_auth_session(NewAuthSession {
        id: "as1".into(),
        user_id: "u1".into(),
        token_hash: hash_token(&admin_token),
        created_at: 1_000_000,
        expires_at: 1_000_000 + 7 * 24 * 60 * 60,
        user_agent: None,
        ip_address: None,
    })
    .await
    .unwrap();

    let state = Arc::new(AppState {
        env_unlock: Arc::new(peckboard::service::env_vars::EnvUnlockRegistry::new()),
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
    // The data dir has to outlive the state; the process is short-lived.
    std::mem::forget(tmp);

    Fixture { state, admin_token }
}

async fn post(state: Arc<AppState>, token: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let router = auth_router(state.clone())
        .merge(repeating_tasks_router(state.clone()))
        .with_state(state);
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

#[tokio::test]
async fn create_user_reports_only_the_constraint_that_failed() {
    let f = build_fixture().await;

    // Username fine, password too short: the message must not mention the
    // username at all.
    let (status, body) = post(
        f.state.clone(),
        &f.admin_token,
        "/api/users",
        serde_json::json!({ "username": "tester2", "password": "abc", "role": "user" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let msg = body["error"].as_str().unwrap();
    assert!(
        msg.contains("password must be at least 12 characters"),
        "expected the password constraint, got {msg:?}"
    );
    assert!(
        !msg.contains("username"),
        "the username was supplied — it must not be named: {msg:?}"
    );
    assert_eq!(body["field"], "password");

    // Username missing, password fine: the mirror case.
    let (status, body) = post(
        f.state.clone(),
        &f.admin_token,
        "/api/users",
        serde_json::json!({ "username": "  ", "password": "correct-horse-battery", "role": "user" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let msg = body["error"].as_str().unwrap();
    assert_eq!(msg, "username is required");
    assert_eq!(body["field"], "username");

    // Both bad: both are reported, and neither is invented.
    let (status, body) = post(
        f.state.clone(),
        &f.admin_token,
        "/api/users",
        serde_json::json!({ "username": "", "password": "abc", "role": "user" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let msg = body["error"].as_str().unwrap();
    assert!(msg.contains("username is required"), "{msg:?}");
    assert!(
        msg.contains("password must be at least 12 characters"),
        "{msg:?}"
    );
}

#[tokio::test]
async fn create_repeating_task_rejects_a_zero_minute_interval() {
    let f = build_fixture().await;

    let (status, body) = post(
        f.state.clone(),
        &f.admin_token,
        "/api/repeating-tasks",
        serde_json::json!({
            "name": "zero",
            "folder_id": "f1",
            "prompt": "do a thing",
            "schedule_kind": "interval",
            "schedule_value": { "minutes": 0 },
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an interval of 0 must be refused, never clamped to 1"
    );
    let msg = body["error"].as_str().unwrap();
    assert!(
        msg.contains("interval minutes must be >= 1"),
        "the reply must state the minimum, got {msg:?}"
    );
}
