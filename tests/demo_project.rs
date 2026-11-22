//! The demo project (`/api/demo-project`) on an install where the Mock
//! plugin is OFF: any signed-in user can create it, its hidden `mock:demo`
//! workers still run (the mock crate registers in demo-only mode) and move
//! a card on, yet no catalog lists a mock model; DELETE removes it and its
//! folder entry.

use std::collections::HashSet;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewAuthSession, NewUser};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::{DEMO_MODEL_ID, ProviderRegistry};
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::worker::orchestrator::check_and_spawn_workers_at;
use peckboard::ws::broadcaster::Broadcaster;
use tower::ServiceExt;

/// State booted like a fresh install with NO crate plugin installed, and a
/// signed-in non-admin user; returns the bearer token.
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
    std::mem::forget(tmp);
    let db = Db::in_memory().unwrap();
    let plugins = Arc::new(PluginManager::new(&config.data_dir, db.clone()));
    let builtin = Arc::new(BuiltinPluginRegistry::new());
    let provider_registry = Arc::new(ProviderRegistry::new());
    peckboard::plugin::crates::register_all(
        &builtin,
        provider_registry.clone(),
        &db,
        plugins.clone(),
        &HashSet::new(),
    )
    .await;
    let session_manager = SessionManager::new(provider_registry.clone());
    let push_service = PushService::new(&config.data_dir);
    let jwt_secret = generate_jwt_secret();

    let now = chrono::Utc::now();
    db.create_user(NewUser {
        id: "u1".into(),
        username: "member".into(),
        email: None,
        password_hash: "h".into(),
        role: "user".into(),
        created_at: now.to_rfc3339(),
        updated_at: now.to_rfc3339(),
    })
    .await
    .unwrap();
    let (token, _exp) = create_token(&jwt_secret, "u1", "user", "as1").unwrap();
    db.create_auth_session(NewAuthSession {
        id: "as1".into(),
        user_id: "u1".into(),
        token_hash: hash_token(&token),
        created_at: now.timestamp(),
        expires_at: now.timestamp() + 7 * 24 * 60 * 60,
        user_agent: None,
        ip_address: None,
    })
    .await
    .unwrap();

    let state = Arc::new(AppState {
        background: Default::default(),
        plugin_ws_tickets: Default::default(),
        device_registry: Default::default(),
        env_unlock: Arc::new(peckboard::service::env_vars::EnvUnlockRegistry::new()),
        config,
        db,
        plugins,
        builtin_plugins: builtin,
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
        remote_access: peckboard::service::remote_access::RemoteAccess::inert(),
        terminals: peckboard::terminal::TerminalManager::inert(),
        tls: Arc::new(peckboard::state::TlsState::new()),
    });
    (state, token)
}

async fn request(
    state: &Arc<AppState>,
    token: &str,
    method: &str,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(method)
        .uri("/api/demo-project")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let app = peckboard::routes::demo_project::router(state.clone()).with_state(state.clone());
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn demo_project_runs_on_the_hidden_demo_model_with_mock_off() {
    let (state, token) = build_state().await;

    // Nothing yet.
    let (status, body) = request(&state, &token, "GET").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_null());

    // A non-admin creates it; a second create returns the same project.
    let (status, created) = request(&state, &token, "POST").await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["name"], "Demo: Plan a small website");
    assert_eq!(created["model"], DEMO_MODEL_ID);
    let (status, again) = request(&state, &token, "POST").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["id"], id.as_str());
    let cards = state.db.list_cards_by_project(&id).await.unwrap();
    assert_eq!(cards.len(), 4);

    // Mock is off: no catalog lists any mock model or the provider itself,
    // but the demo model still dispatches.
    let all = state.provider_registry.list_all_models().await;
    assert!(
        all.iter().all(|(m, _)| !m.starts_with("mock:")),
        "mock models leaked into the catalog: {all:?}"
    );
    let listed = state.provider_registry.list_providers_with_models().await;
    assert!(listed.iter().all(|p| p.id != "mock"));
    assert!(state.provider_registry.get_provider("mock").await.is_some());

    // A worker on mock:demo picks a card up and its scripted turn moves it
    // on (in progress → review) through the real finish_card tool.
    check_and_spawn_workers_at(&state, chrono::Utc::now()).await;
    let mut advanced = false;
    for _ in 0..200 {
        let cards = state.db.list_cards_by_project(&id).await.unwrap();
        if cards.iter().any(|c| c.step == "review" || c.step == "done") {
            advanced = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(advanced, "no demo card moved past in progress");

    // Delete removes the project and its folder entry.
    let folder_id = state.db.get_project(&id).await.unwrap().unwrap().folder_id;
    let (status, _) = request(&state, &token, "DELETE").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(state.db.get_project(&id).await.unwrap().is_none());
    assert!(state.db.get_folder(&folder_id).await.unwrap().is_none());
    let (status, body) = request(&state, &token, "GET").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_null());
}
