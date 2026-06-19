//! HTTP-level tests for the public, plugin-owned `/plugin-api/*` surface
//! added by the "plugin-served HTTP routes" core ticket. These drive the
//! real `peckboard::routes::api_router` via `tower::ServiceExt::oneshot`
//! against `Db::in_memory()`.
//!
//! They prove the *generic plumbing* this ticket owns:
//! - an unclaimed `/plugin-api/*` path returns 404 (no loaded plugin
//!   declares a matching route),
//! - the `/plugin-api/*` prefix is reachable WITHOUT a session token —
//!   it is intentionally not behind the `/api/*` auth middleware (the
//!   serving plugin owns its own auth), and
//! - existing `/api/*` auth is unchanged: a protected route still 401s
//!   without a token.
//!
//! The end-to-end dispatch-through-a-real-WASM-plugin path (a declared
//! route returning the plugin's `{status, headers, body}` response) is
//! covered by unit tests in `src/plugin/manager.rs` — `verdict_to_outcome`
//! and `match_http_route` are exercised directly there — because this
//! repo's `cargo test` has no `wasm32` toolchain to compile a fixture
//! plugin. The compiled API plugin (sibling ticket) verifies the full
//! loaded-plugin round trip from its own crate.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::generate_jwt_secret;
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::routes::api_router;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::ws::broadcaster::Broadcaster;
use tower::ServiceExt;

async fn build_state() -> Arc<AppState> {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        port: 0,
        https_port: 0,
        host: "127.0.0.1".into(),
        data_dir: tmp.path().to_path_buf(),
        mdns: false,
    };

    let db = Db::in_memory().unwrap();
    // No plugins are loaded — `empty()` hosts none, which is exactly the
    // "no plugin claims the route" condition we want to assert 404s on.
    let plugins = Arc::new(PluginManager::empty());
    let jwt_secret = generate_jwt_secret();
    let provider_registry = Arc::new(ProviderRegistry::new());
    let session_manager = SessionManager::new(provider_registry.clone());
    let push_service = PushService::new(&config.data_dir);

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
    });

    std::mem::forget(tmp);
    state
}

/// An unclaimed `/plugin-api/*` path returns 404 with a JSON error — and
/// crucially NOT 401: the request reaches the dispatcher (proving the
/// prefix bypasses the `/api/*` auth middleware) and simply finds no
/// plugin route.
#[tokio::test]
async fn unclaimed_plugin_api_path_returns_404_not_401() {
    let state = build_state().await;

    let req = Request::builder()
        .method("GET")
        .uri("/plugin-api/v1/anything/here")
        .body(Body::empty())
        .unwrap();

    let resp = api_router(state.clone())
        .with_state(state)
        .oneshot(req)
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "no plugin claims the route, and the prefix is not auth-gated, so it must be 404 (not 401)"
    );

    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        json.get("error").is_some(),
        "404 body carries an error field"
    );
}

/// The bare `/plugin-api` prefix (no trailing path) is also handled by
/// the dispatcher rather than falling through to the SPA static handler
/// or an auth gate.
#[tokio::test]
async fn bare_plugin_api_prefix_is_handled() {
    let state = build_state().await;

    let req = Request::builder()
        .method("POST")
        .uri("/plugin-api")
        .body(Body::empty())
        .unwrap();

    let resp = api_router(state.clone())
        .with_state(state)
        .oneshot(req)
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Guardrail: the new public prefix must not have weakened existing
/// `/api/*` protection. A known auth-gated route still 401s without a
/// token.
#[tokio::test]
async fn existing_api_routes_still_require_auth() {
    let state = build_state().await;

    let req = Request::builder()
        .method("GET")
        .uri("/api/plugins")
        .body(Body::empty())
        .unwrap();

    let resp = api_router(state.clone())
        .with_state(state)
        .oneshot(req)
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "/api/* must still be auth-gated"
    );
}

/// Sanity: the same auth-gated route, hit with a junk bearer token, is
/// still rejected — the public-prefix change didn't open a bypass.
#[tokio::test]
async fn api_route_rejects_bad_token() {
    let state = build_state().await;

    let req = Request::builder()
        .method("GET")
        .uri("/api/plugins")
        .header(header::AUTHORIZATION, "Bearer not-a-real-token")
        .body(Body::empty())
        .unwrap();

    let resp = api_router(state.clone())
        .with_state(state)
        .oneshot(req)
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
