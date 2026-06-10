//! HTTP-level test for `POST /api/sessions/:id/terminate`.
//!
//! The route is meant to be used between turns — when the long-lived
//! provider child is alive but no turn is active — so the user can force
//! a fresh process spawn (to reload skills, MCP config, etc) on the next
//! message. This test pins the contract:
//!
//! 1. Returns 204 when the session exists.
//! 2. Appends a `system` event whose `text` mentions termination, so the
//!    user sees confirmation in the transcript even when no in-flight
//!    turn would have emitted a `Crashed` event of its own.
//! 3. Returns 404 for an unknown session id.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewAuthSession, NewFolder, NewSession, NewUser};
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
    let plugins = Arc::new(PluginManager::new(&config.data_dir));
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

    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: "f1".into(),
        name: "F".into(),
        path: "/tmp/f".into(),
        created_at: ts.clone(),
    })
    .await
    .unwrap();
    db.create_session(NewSession {
        id: "s1".into(),
        name: "Chat".into(),
        folder_id: "f1".into(),
        created_at: ts.clone(),
        last_activity: ts,
        ..Default::default()
    })
    .await
    .unwrap();

    let state = Arc::new(AppState {
        config,
        db,
        plugins,
        jwt_secret,
        login_limiter: RateLimiter::new(60),
        password_change_limiter: RateLimiter::<String>::new(5),
        broadcaster: Broadcaster::new(),
        provider_registry,
        session_manager,
        repeating_task_manager: peckboard::repeating::RepeatingTaskManager::new(),
        mcp_tokens: McpTokenRegistry::new(),
        push_service,
        pm_authorizations: Default::default(),
    });

    std::mem::forget(tmp);
    (state, token)
}

#[tokio::test]
async fn terminate_returns_204_and_appends_system_notice() {
    let (state, token) = build_state().await;

    let req = Request::builder()
        .method("POST")
        .uri("/api/sessions/s1/terminate")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = router(state.clone())
        .with_state(state.clone())
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // A `system` event must be on the tail telling the user the agent
    // was terminated. Without it the kebab-menu action looks silent in
    // the transcript when no turn was active.
    let events = state.db.events_tail("s1", 10).await.unwrap();
    let system_event = events
        .iter()
        .find(|e| e.kind == "system")
        .expect("terminate must append a system event");
    let data: serde_json::Value = serde_json::from_str(&system_event.data).unwrap();
    let text = data.get("text").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        text.contains("terminated"),
        "system event should mention termination, got {text:?}"
    );
}

#[tokio::test]
async fn terminate_returns_404_for_unknown_session() {
    let (state, token) = build_state().await;

    let req = Request::builder()
        .method("POST")
        .uri("/api/sessions/does-not-exist/terminate")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = router(state.clone())
        .with_state(state)
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
