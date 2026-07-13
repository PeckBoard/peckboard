//! Tests for per-provider hidden-flag settings endpoints and catalog filtering.
//!
//! Covers:
//! - GET /api/settings/providers — default: all visible
//! - PUT /api/settings/providers/{id} — unknown id → 404; round-trip
//! - GET /api/models — hidden providers filtered from providers + models arrays
//! - registry.list_providers() — unaffected by the hidden flag

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
use peckboard::provider::mock::MockProvider;
use peckboard::provider::registry::{ProviderInfo, ProviderRegistry};
use peckboard::provider::stream::ModelInfo;
use peckboard::routes::misc::router as misc_router;
use peckboard::routes::settings::router as settings_router;
use peckboard::service::mcp_server::McpTokenRegistry;
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
        keep_alive_hours: 0,
        provider_send_timeout_secs: 300,
    };
    let db = Db::in_memory().unwrap();
    let plugins = Arc::new(PluginManager::new(&config.data_dir, db.clone()));
    let jwt_secret = generate_jwt_secret();
    let provider_registry = Arc::new(ProviderRegistry::new());

    provider_registry
        .register(
            Arc::new(MockProvider::new()),
            ProviderInfo {
                id: "mock".into(),
                display_name: "Mock".into(),
                models: vec![ModelInfo {
                    id: "happy-path".into(),
                    display_name: "Happy Path".into(),
                    capabilities: vec![],
                    tier: 0,
                }],
                effort_levels: vec![],
            },
        )
        .await;

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

fn make_router(state: Arc<AppState>) -> axum::Router {
    settings_router(state.clone())
        .merge(misc_router(state.clone()))
        .with_state(state)
}

async fn get_json(state: Arc<AppState>, token: &str, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = make_router(state).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn put_json(state: Arc<AppState>, token: &str, uri: &str, body: Value) -> StatusCode {
    let req = Request::builder()
        .method(Method::PUT)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = make_router(state).oneshot(req).await.unwrap();
    resp.status()
}

#[tokio::test]
async fn providers_list_default_all_visible() {
    let (state, token) = build_state().await;
    let (status, body) = get_json(state, &token, "/api/settings/providers").await;
    assert_eq!(status, StatusCode::OK);
    let providers = body["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0]["id"], "mock");
    assert_eq!(providers[0]["hidden"], false);
}

#[tokio::test]
async fn providers_put_unknown_id_returns_404() {
    let (state, token) = build_state().await;
    let status = put_json(
        state,
        &token,
        "/api/settings/providers/nonexistent",
        serde_json::json!({ "hidden": true }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn providers_put_hidden_round_trips() {
    let (state, token) = build_state().await;

    // Hide.
    let st = put_json(
        state.clone(),
        &token,
        "/api/settings/providers/mock",
        serde_json::json!({ "hidden": true }),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    let (_, body) = get_json(state.clone(), &token, "/api/settings/providers").await;
    let p = &body["providers"].as_array().unwrap()[0];
    assert_eq!(p["id"], "mock");
    assert_eq!(p["hidden"], true, "provider should be hidden after PUT");

    // Un-hide.
    let st2 = put_json(
        state.clone(),
        &token,
        "/api/settings/providers/mock",
        serde_json::json!({ "hidden": false }),
    )
    .await;
    assert_eq!(st2, StatusCode::NO_CONTENT);

    let (_, body2) = get_json(state, &token, "/api/settings/providers").await;
    let p2 = &body2["providers"].as_array().unwrap()[0];
    assert_eq!(
        p2["hidden"], false,
        "provider should be visible again after un-hiding"
    );
}

#[tokio::test]
async fn hidden_provider_filtered_from_models_not_from_registry() {
    let (state, token) = build_state().await;

    // Before: mock present in /api/models.
    let (_, before) = get_json(state.clone(), &token, "/api/models").await;
    assert!(
        before["providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == "mock"),
        "mock should appear in /api/models before hiding"
    );
    assert!(
        before["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"].as_str().unwrap_or("").starts_with("mock:")),
        "mock models should appear in /api/models before hiding"
    );

    // Hide mock.
    let st = put_json(
        state.clone(),
        &token,
        "/api/settings/providers/mock",
        serde_json::json!({ "hidden": true }),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    // After: mock absent from /api/models.
    let (_, after) = get_json(state.clone(), &token, "/api/models").await;
    assert!(
        !after["providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == "mock"),
        "mock should be absent from /api/models providers after hiding"
    );
    assert!(
        !after["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"].as_str().unwrap_or("").starts_with("mock:")),
        "mock models should be absent from /api/models after hiding"
    );

    // Registry unaffected.
    let registry_list = state.provider_registry.list_providers().await;
    assert!(
        registry_list.iter().any(|p| p.id == "mock"),
        "registry.list_providers() must still contain mock regardless of hidden flag"
    );
}
