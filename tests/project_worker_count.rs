//! Regression tests for the per-project worker cap.
//!
//! `Project.worker_count` is an `i32` that the orchestrator casts to
//! `usize`. A negative value casts to `usize::MAX`, so the cap check never
//! trips and every eligible card spawns a worker at once. The write
//! boundaries must refuse a negative value, and `db` clamps as a backstop.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewAuthSession, NewFolder, NewProject, NewUser, UpdateProject};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::routes::projects::router as projects_router;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::ws::broadcaster::Broadcaster;
use serde_json::{Value, json};
use tower::ServiceExt;

const PROJECT: &str = "proj-1";

struct Fixture {
    state: Arc<AppState>,
    token: String,
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
        updated_at: now.clone(),
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

    db.create_folder(NewFolder {
        id: "folder-1".into(),
        name: "folder-1".into(),
        path: tmp.path().join("folder-1").to_string_lossy().into_owned(),
        created_at: now.clone(),
    })
    .await
    .unwrap();
    db.create_project(NewProject {
        id: PROJECT.into(),
        name: PROJECT.into(),
        context: String::new(),
        folder_id: "folder-1".into(),
        worker_count: 1,
        status: "active".into(),
        workflow: "task".into(),
        model: None,
        effort: None,
        budget_usd_cents: None,
        budget_period: None,
        worktree_isolation: false,
        parallel_instructions: false,
        auto_notify_changes: true,
        worker_communication: false,
        created_at: now.clone(),
        last_accessed_at: now.clone(),
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
        ssh_vault_key: vec![0u8; 32],
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
    // The data dir has to outlive the state; the process is short-lived.
    std::mem::forget(tmp);

    Fixture { state, token }
}

async fn call(f: &Fixture, method: Method, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {}", f.token))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = projects_router(f.state.clone())
        .with_state(f.state.clone())
        .oneshot(req)
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

#[tokio::test]
async fn put_project_rejects_negative_worker_count() {
    let f = build_fixture().await;

    let (status, body) = call(
        &f,
        Method::PUT,
        &format!("/api/projects/{PROJECT}"),
        json!({ "worker_count": -1, "workflow": "task" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");

    let project = f.state.db.get_project(PROJECT).await.unwrap().unwrap();
    assert_eq!(project.worker_count, 1, "the cap must be left untouched");
}

#[tokio::test]
async fn post_project_rejects_negative_worker_count() {
    let f = build_fixture().await;

    let (status, body) = call(
        &f,
        Method::POST,
        "/api/projects",
        json!({
            "name": "neg",
            "folder_id": "folder-1",
            "workflow": "task",
            "worker_count": -3,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(f.state.db.list_projects().await.unwrap().len(), 1);
}

/// Backstop: even a direct db write can't persist a negative cap, so
/// `worker_count as usize` in the orchestrator can never be `usize::MAX`.
#[tokio::test]
async fn db_clamps_negative_worker_count() {
    let f = build_fixture().await;

    let updated = f
        .state
        .db
        .update_project(
            PROJECT,
            UpdateProject {
                worker_count: Some(-1),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.worker_count, 0);
    assert_eq!(updated.worker_count.max(0) as usize, 0);
}
