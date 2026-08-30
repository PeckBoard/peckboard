//! Regression tests for the account-delete reference guard
//! (`routes/account_delete_guard.rs`) and `set_default_model` validation
//! (`routes/settings.rs`): a delete must not silently strand sessions/
//! cards/default_model pinned to the account, and the default-model setting
//! must not accept an unknown model id.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewAuthSession, NewClaudeAccount, NewFolder, NewSession, NewUser};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::mock::register_mock_provider;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::routes::claude_accounts::router as claude_accounts_router;
use peckboard::routes::settings::router as settings_router;
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
    register_mock_provider(&provider_registry).await;
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
        plugin_ws_tickets: Default::default(),
        env_unlock: Arc::new(peckboard::service::env_vars::EnvUnlockRegistry::new()),
        config,
        db,
        plugins,
        builtin_plugins: Arc::new(BuiltinPluginRegistry::new()),
        jwt_secret,
        ssh_vault_key: vec![0u8; 32],
        mfa_vault_key: vec![0u8; 32],
        login_limiter: RateLimiter::new(60),
        password_change_limiter: RateLimiter::<String>::new(5),
        broadcaster: Broadcaster::new(),
        provider_registry,
        session_manager,
        repeating_task_manager: peckboard::repeating::RepeatingTaskManager::new(),
        run_auditor: peckboard::repeating::RunAuditor::new(),
        mcp_tokens: McpTokenRegistry::new(),
        push_service,
        tls: Arc::new(peckboard::state::TlsState::new()),
    });
    std::mem::forget(tmp);

    Fixture { state, admin_token }
}

fn make_router(state: Arc<AppState>) -> axum::Router {
    claude_accounts_router(state.clone())
        .merge(settings_router(state.clone()))
        .with_state(state)
}

async fn call(
    state: Arc<AppState>,
    token: &str,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    let req = match body {
        Some(v) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            builder.body(Body::empty()).unwrap()
        }
    };
    let resp = make_router(state).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn seed_account(db: &Db, id: &str) {
    db.create_claude_account(NewClaudeAccount {
        id: id.into(),
        name: id.into(),
        kind: "oauth".into(),
        credential: "secret".into(),
        config_dir: None,
        budget_window_hours: None,
        budget_limit_usd: None,
        budget_limit_tokens: None,
        warn_threshold: 0.8,
        critical_threshold: 0.95,
        created_at: 1,
        updated_at: 1,
        refresh_token: None,
        token_expires_at: None,
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn delete_with_no_refs_succeeds_immediately() {
    let f = build_fixture().await;
    seed_account(&f.state.db, "acc_unused").await;

    let (status, _) = call(
        f.state.clone(),
        &f.admin_token,
        Method::DELETE,
        "/api/claude-accounts/acc_unused",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        f.state
            .db
            .get_claude_account("acc_unused")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn delete_with_pinned_session_is_refused_without_force() {
    let f = build_fixture().await;
    seed_account(&f.state.db, "acc_pinned").await;

    let ts = chrono::Utc::now().to_rfc3339();
    f.state
        .db
        .create_folder(NewFolder {
            id: "f1".into(),
            name: "f1".into(),
            path: "/tmp/account-delete-guard-test".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
    f.state
        .db
        .create_session(NewSession {
            id: "s1".into(),
            name: "s1".into(),
            folder_id: "f1".into(),
            model: Some("mock:echo@acc_pinned".into()),
            created_at: ts.clone(),
            last_activity: ts,
            ..Default::default()
        })
        .await
        .unwrap();

    let (status, body) = call(
        f.state.clone(),
        &f.admin_token,
        Method::DELETE,
        "/api/claude-accounts/acc_pinned",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["sessions"], serde_json::json!(["s1"]));
    // Refused: the row and the reference must both still be intact.
    assert!(
        f.state
            .db
            .get_claude_account("acc_pinned")
            .await
            .unwrap()
            .is_some()
    );

    let (status, _) = call(
        f.state.clone(),
        &f.admin_token,
        Method::DELETE,
        "/api/claude-accounts/acc_pinned?force=true",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        f.state
            .db
            .get_claude_account("acc_pinned")
            .await
            .unwrap()
            .is_none()
    );
    let s1 = f.state.db.get_session("s1").await.unwrap().unwrap();
    assert_eq!(
        s1.model.as_deref(),
        Some("mock:echo"),
        "force delete should strip the @account suffix, not leave a dangling reference"
    );
}

#[tokio::test]
async fn delete_clears_a_pinned_default_model_on_force() {
    let f = build_fixture().await;
    seed_account(&f.state.db, "acc_default").await;

    // Point the app-wide default at this account directly (bypassing the
    // registry-validated route, since a claude-provider@account model id
    // isn't in the mock provider's catalog).
    let value = serde_json::json!({ "model": "claude-opus-4-8@acc_default" }).to_string();
    f.state
        .db
        .plugin_store_put_blocking("core.settings", "app", "default_model", &value)
        .unwrap();

    let (status, body) = call(
        f.state.clone(),
        &f.admin_token,
        Method::DELETE,
        "/api/claude-accounts/acc_default",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["default_model"], true);

    let (status, _) = call(
        f.state.clone(),
        &f.admin_token,
        Method::DELETE,
        "/api/claude-accounts/acc_default?force=true",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, body) = call(
        f.state.clone(),
        &f.admin_token,
        Method::GET,
        "/api/settings/default-model",
        None,
    )
    .await;
    assert_eq!(body["model"], "");
}

#[tokio::test]
async fn set_default_model_rejects_unknown_model() {
    let f = build_fixture().await;

    let (status, body) = call(
        f.state.clone(),
        &f.admin_token,
        Method::PUT,
        "/api/settings/default-model",
        Some(serde_json::json!({ "model": "mock:definitely-not-a-real-model" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("unknown model"));
}

#[tokio::test]
async fn set_default_model_accepts_known_model_and_clear() {
    let f = build_fixture().await;

    let (status, _) = call(
        f.state.clone(),
        &f.admin_token,
        Method::PUT,
        "/api/settings/default-model",
        Some(serde_json::json!({ "model": "mock:echo" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = call(
        f.state.clone(),
        &f.admin_token,
        Method::PUT,
        "/api/settings/default-model",
        Some(serde_json::json!({ "model": "" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}
