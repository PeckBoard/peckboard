//! Worker sessions are owned by their card / project. The orchestrator
//! creates them when a card is picked up and the project / card cascade
//! deletes them when the parent goes away. Letting a user delete one
//! directly via `DELETE /api/sessions/:id` would leave the card pointing
//! at a vanished `worker_session_id` and bypass the orchestrator's
//! bookkeeping — so the route refuses with 409.
//!
//! This file locks that contract in: a plain chat session still deletes
//! cleanly, and a worker session 409s and stays in the DB.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewAuthSession, NewCard, NewFolder, NewProject, NewSession, NewUser};
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
        keep_alive_hours: 0,
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

    // A plain chat session — should be deletable.
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

    // A worker session for the card — should NOT be deletable directly.
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
}

async fn delete_session(state: Arc<AppState>, token: &str, id: &str) -> StatusCode {
    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/api/sessions/{id}"))
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
async fn worker_session_delete_is_refused_and_row_survives() {
    let (state, token) = build_state().await;
    seed(&state).await;

    let status = delete_session(state.clone(), &token, "worker").await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "DELETE on a worker session must be refused with 409",
    );

    // The row is still in the DB — the orchestrator's bookkeeping
    // depends on the card's `worker_session_id` continuing to resolve.
    let still_there = state.db.get_session("worker").await.unwrap();
    assert!(
        still_there.is_some(),
        "worker session must survive the rejected delete",
    );
}

#[tokio::test]
async fn plain_session_still_deletes_cleanly() {
    let (state, token) = build_state().await;
    seed(&state).await;

    let status = delete_session(state.clone(), &token, "plain").await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "DELETE on a plain chat session must still succeed",
    );

    let gone = state.db.get_session("plain").await.unwrap();
    assert!(gone.is_none(), "plain session must be removed");
}

#[tokio::test]
async fn plain_session_delete_broadcasts_session_deleted() {
    // Without this broadcast, another device with the now-deleted session
    // open keeps showing its ChatView body — the only cross-device cleanup
    // path is the focus-driven `/api/me/tabs` refetch, which closes the
    // tab strip entry but leaves the body pointed at a 404'd session id.
    let (state, token) = build_state().await;
    seed(&state).await;

    let mut rx = state.broadcaster.subscribe_all();
    let status = delete_session(state.clone(), &token, "plain").await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Drain at most a handful of frames looking for `session-deleted` —
    // the route can emit unrelated events first (interrupt cleanup, etc).
    let mut found = false;
    for _ in 0..8 {
        match tokio::time::timeout(std::time::Duration::from_millis(250), rx.recv()).await {
            Ok(Ok(event)) => {
                if event.event_type == "session-deleted" && event.session_id == "plain" {
                    found = true;
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(
        found,
        "DELETE /api/sessions/:id must broadcast a session-deleted frame",
    );
}
