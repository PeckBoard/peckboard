//! HTTP-level tests for `/api/workflows` CRUD (custom workflows layered
//! on top of the hardcoded built-ins in `src/workflow.rs`).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewAuthSession, NewFolder, NewProject, NewUser};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::routes::workflows::router;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;

/// `workflow::CUSTOM_WORKFLOWS` is a single process-wide static that every
/// mutation reloads wholesale from `state.db` — correct for production
/// (one real DB) but not safe under `cargo test`'s default parallel test
/// execution, where each test in this file builds its OWN in-memory DB.
/// Two tests reloading concurrently would stomp each other's registry
/// state. Serialize this file's tests on a plain mutex instead.
static REGISTRY_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());
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
        keep_alive_hours: 0,
        provider_send_timeout_secs: 300,
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

    std::mem::forget(tmp);
    (state, token)
}

fn create_body() -> serde_json::Value {
    serde_json::json!({
        "name": "My Custom Flow",
        "description": "a custom flow",
        "steps": [
            {"step": "backlog", "instructions": ""},
            {"step": "in_progress", "instructions": "Do the thing."},
            {"step": "done", "instructions": ""},
        ],
    })
}

async fn post_workflow(
    state: &Arc<AppState>,
    token: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/api/workflows")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router(state.clone())
        .with_state(state.clone())
        .oneshot(req)
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn create_custom_workflow_appears_in_list_tagged_custom() {
    let _guard = REGISTRY_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    peckboard::workflow::set_custom_workflows(Vec::new());
    let (state, token) = build_state().await;

    let (status, created) = post_workflow(&state, &token, create_body()).await;
    assert_eq!(status, StatusCode::CREATED, "{created:?}");
    let id = created
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();
    assert_eq!(
        created.get("source").and_then(|v| v.as_str()),
        Some("custom")
    );

    let req = Request::builder()
        .method("GET")
        .uri("/api/workflows")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = router(state.clone())
        .with_state(state.clone())
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let workflows = json.get("workflows").and_then(|v| v.as_array()).unwrap();
    let entry = workflows
        .iter()
        .find(|w| w.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
        .expect("custom workflow must appear in the merged list");
    assert_eq!(entry.get("source").and_then(|v| v.as_str()), Some("custom"));
    let builtin = workflows
        .iter()
        .find(|w| w.get("id").and_then(|v| v.as_str()) == Some("task"))
        .expect("built-in workflow must still be listed");
    assert_eq!(
        builtin.get("source").and_then(|v| v.as_str()),
        Some("builtin")
    );
}

#[tokio::test]
async fn create_rejects_name_colliding_with_builtin() {
    let _guard = REGISTRY_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    peckboard::workflow::set_custom_workflows(Vec::new());
    let (state, token) = build_state().await;
    let mut body = create_body();
    body["name"] = serde_json::Value::String("Task".into());
    let (status, err) = post_workflow(&state, &token, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err:?}");
}

#[tokio::test]
async fn create_rejects_malformed_steps() {
    let _guard = REGISTRY_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    peckboard::workflow::set_custom_workflows(Vec::new());
    let (state, token) = build_state().await;
    let mut body = create_body();
    body["steps"] = serde_json::json!([{"step": "in_progress", "instructions": "x"}]);
    let (status, err) = post_workflow(&state, &token, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err:?}");
}

#[tokio::test]
async fn delete_blocked_by_referencing_project_returns_409_with_details() {
    let _guard = REGISTRY_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    peckboard::workflow::set_custom_workflows(Vec::new());
    let (state, token) = build_state().await;
    let (status, created) = post_workflow(&state, &token, create_body()).await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

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
            name: "Uses Custom Flow".into(),
            context: "".into(),
            folder_id: "f1".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: id.clone(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: false,
            worker_communication: false,
            created_at: ts.clone(),
            worktree_isolation: false,
            last_accessed_at: ts,
            budget_usd_cents: None,
            budget_period: None,
        })
        .await
        .unwrap();

    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/api/workflows/{id}"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = router(state.clone())
        .with_state(state.clone())
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let projects = json.get("projects").and_then(|v| v.as_array()).unwrap();
    assert!(
        projects
            .iter()
            .any(|p| p.get("id").and_then(|v| v.as_str()) == Some("p1")),
        "409 body must list the referencing project: {json:?}"
    );
}

#[tokio::test]
async fn delete_unreferenced_custom_workflow_succeeds() {
    let _guard = REGISTRY_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    peckboard::workflow::set_custom_workflows(Vec::new());
    let (state, token) = build_state().await;
    let (status, created) = post_workflow(&state, &token, create_body()).await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/api/workflows/{id}"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = router(state.clone())
        .with_state(state.clone())
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn put_and_delete_reject_builtin_ids() {
    let _guard = REGISTRY_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    peckboard::workflow::set_custom_workflows(Vec::new());
    let (state, token) = build_state().await;

    let req = Request::builder()
        .method("PUT")
        .uri("/api/workflows/task")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(create_body().to_string()))
        .unwrap();
    let resp = router(state.clone())
        .with_state(state.clone())
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let req = Request::builder()
        .method("DELETE")
        .uri("/api/workflows/task")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = router(state.clone())
        .with_state(state.clone())
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
