//! HTTP-level tests for the plugin hook-approval surface added so a
//! WASM plugin stays inert until an operator approves its declared hooks.
//!
//! The full loaded-plugin round trip (a real `.wasm` going pending →
//! approved → active) needs a `wasm32` toolchain this repo's `cargo test`
//! doesn't have, so it's covered by the Playwright e2e (with the
//! non-WASM plugin stub) and the sibling plugin crate. Here we lock in the
//! generic plumbing: the catalog always carries a `wasm_plugins` array,
//! and `POST /api/plugins/:id/approval` validates its input and 404s for
//! an unknown plugin.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewAuthSession, NewUser};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::builtins::register_all as register_builtin_plugins;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::routes::plugins::router;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::ws::broadcaster::Broadcaster;
use serde_json::{Value, json};
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
    let builtin_plugins = Arc::new(BuiltinPluginRegistry::new());
    register_builtin_plugins(&builtin_plugins, provider_registry.clone(), db.clone()).await;
    let session_manager = SessionManager::new(provider_registry.clone());
    let push_service = PushService::new(&config.data_dir);

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
        created_at: 1_000_000,
        expires_at: 1_000_000 + 7 * 24 * 60 * 60,
        user_agent: None,
        ip_address: None,
    })
    .await
    .unwrap();

    let state = Arc::new(AppState {
        config,
        db,
        plugins,
        builtin_plugins,
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

#[tokio::test]
async fn catalog_always_carries_wasm_plugins_array() {
    let (state, token) = build_state().await;
    let app = router(state.clone()).with_state(state.clone());

    let req = Request::builder()
        .uri("/api/plugins")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    // No WASM plugins loaded here, but the field must always be a present
    // array — the approval prompt reads it unconditionally.
    assert!(
        json["wasm_plugins"].is_array(),
        "wasm_plugins must be an array, got {:?}",
        json["wasm_plugins"]
    );
    assert_eq!(json["wasm_plugins"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn approval_rejects_bad_decision() {
    let (state, token) = build_state().await;
    let app = router(state.clone()).with_state(state.clone());

    let req = Request::builder()
        .method("POST")
        .uri("/api/plugins/api/approval")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "decision": "maybe" }).to_string()))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn approval_404s_for_unknown_plugin() {
    let (state, token) = build_state().await;
    let app = router(state.clone()).with_state(state.clone());

    // Valid decision, but no plugin named `ghost` is loaded.
    let req = Request::builder()
        .method("POST")
        .uri("/api/plugins/ghost/approval")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "decision": "approve" }).to_string()))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn registry_install_rejects_missing_id() {
    let (state, token) = build_state().await;
    let app = router(state.clone()).with_state(state.clone());

    // `id` is validated before any registry fetch, so this is deterministic
    // and needs no network.
    let req = Request::builder()
        .method("POST")
        .uri("/api/plugins/registry/install")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({}).to_string()))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn repositories_add_list_remove() {
    let (state, token) = build_state().await;
    let app = router(state.clone());

    // Default seed is present.
    let req = Request::builder()
        .uri("/api/plugins/repositories")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = app
        .clone()
        .with_state(state.clone())
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    let repos = json["repositories"].as_array().unwrap();
    assert!(repos.iter().any(|r| r["label"] == "PeckBoard/plugins"));

    // Add by slug → resolves to the GitHub raw URL.
    let req = Request::builder()
        .method("POST")
        .uri("/api/plugins/repositories")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "repository": "octo/cat" }).to_string()))
        .unwrap();
    let res = app
        .clone()
        .with_state(state.clone())
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    let url = json["repository"]["url"].as_str().unwrap().to_string();
    assert_eq!(
        url,
        "https://raw.githubusercontent.com/octo/cat/main/registry.json"
    );

    // Invalid input → 400.
    let req = Request::builder()
        .method("POST")
        .uri("/api/plugins/repositories")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "repository": "not a repo" }).to_string(),
        ))
        .unwrap();
    let res = app
        .clone()
        .with_state(state.clone())
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // Remove the added repo.
    let req = Request::builder()
        .method("DELETE")
        .uri("/api/plugins/repositories")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "url": url }).to_string()))
        .unwrap();
    let res = app
        .clone()
        .with_state(state.clone())
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // Removing an unknown url → 404.
    let req = Request::builder()
        .method("DELETE")
        .uri("/api/plugins/repositories")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "url": "https://nope.example/r.json" }).to_string(),
        ))
        .unwrap();
    let res = app
        .clone()
        .with_state(state.clone())
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn install_unknown_repository_is_404() {
    let (state, token) = build_state().await;
    let app = router(state.clone()).with_state(state.clone());

    // A repository url that isn't configured → 404 before any network.
    let req = Request::builder()
        .method("POST")
        .uri("/api/plugins/registry/install")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "id": "api", "repository": "https://nope.example/r.json" }).to_string(),
        ))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn uninstall_unknown_plugin_is_404() {
    let (state, token) = build_state().await;
    let app = router(state.clone()).with_state(state.clone());

    // No WASM plugin named `ghost` is loaded, so DELETE 404s. (The full
    // load → uninstall round trip needs a real `.wasm` and is covered by the
    // Playwright e2e and the sibling plugin crate.)
    let req = Request::builder()
        .method("DELETE")
        .uri("/api/plugins/ghost")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn uninstall_rejects_unsafe_id() {
    let (state, token) = build_state().await;
    let app = router(state.clone()).with_state(state.clone());

    // An id outside `^[a-z0-9_-]+$` (here: a dot) is rejected before any
    // filesystem access → 400, not 404.
    let req = Request::builder()
        .method("DELETE")
        .uri("/api/plugins/a.b")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn uninstall_requires_auth() {
    let (state, _token) = build_state().await;
    let app = router(state.clone()).with_state(state.clone());

    let req = Request::builder()
        .method("DELETE")
        .uri("/api/plugins/api")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn registry_endpoints_require_auth() {
    let (state, _token) = build_state().await;
    let app = router(state.clone()).with_state(state.clone());

    let req = Request::builder()
        .uri("/api/plugins/registry")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn approval_requires_auth() {
    let (state, _token) = build_state().await;
    let app = router(state.clone()).with_state(state.clone());

    let req = Request::builder()
        .method("POST")
        .uri("/api/plugins/api/approval")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "decision": "approve" }).to_string()))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}
