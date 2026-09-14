//! Interrupt/terminate vs a queued follow-up, driven through the real HTTP
//! routes and a replica of the `main.rs` completion listener.
//!
//! Originally an Ollama firehose test. The bug is in the completion listener
//! (explicit stop respawning from the queue), not in any one provider — so
//! this now uses the mock WASM plugin (`mock:ask` blocks on stdin, same as a
//! streaming turn staying alive).

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewAuthSession, NewFolder, NewSession, NewUser};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::provider::stream::SpawnConfig;
use peckboard::routes::sessions::router;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::ws::broadcaster::Broadcaster;
use tower::ServiceExt;

mod common;

fn config() -> SpawnConfig {
    SpawnConfig {
        model: "mock:ask".into(),
        working_dir: "/tmp/f".into(),
        ..Default::default()
    }
}

async fn build_state() -> (Arc<AppState>, String) {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config {
        port: 0,
        https_port: 0,
        host: "127.0.0.1".into(),
        data_dir: tmp.path().to_path_buf(),
        mdns: false,
        keep_alive_hours: 0,
        provider_send_timeout_secs: 300,
    };
    let db = Db::in_memory().unwrap();
    let provider_registry = Arc::new(ProviderRegistry::new());
    let plugins =
        common::load_first_party_providers(&cfg.data_dir, db.clone(), &provider_registry).await;
    assert!(
        provider_registry.get_info("mock").await.is_some(),
        "mock WASM plugin must register"
    );

    let session_manager =
        SessionManager::new(provider_registry.clone()).with_plugins(plugins.clone());
    let completion_rx = session_manager.take_completion_rx().await.unwrap();
    let jwt_secret = generate_jwt_secret();

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
        model: Some("mock:ask".into()),
        created_at: ts.clone(),
        last_activity: ts,
        ..Default::default()
    })
    .await
    .unwrap();

    let state = Arc::new(AppState {
        plugin_ws_tickets: Default::default(),
        env_unlock: Arc::new(peckboard::service::env_vars::EnvUnlockRegistry::new()),
        config: cfg,
        db: db.clone(),
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
        push_service: PushService::new(tmp.path()),
        tls: Arc::new(peckboard::state::TlsState::new()),
    });
    std::mem::forget(tmp);

    {
        let listener_state = state.clone();
        let mut rx = completion_rx;
        tokio::spawn(async move {
            while let Some(completion) = rx.recv().await {
                let sid = completion.session_id.clone();
                let _ =
                    peckboard::worker::orchestrator::drain_queue_for_session(&listener_state, &sid)
                        .await;
            }
        });
    }

    (state, token)
}

async fn send_and_queue_followup(state: &Arc<AppState>) {
    state
        .session_manager
        .send_or_queue(
            "s1",
            "first".into(),
            &state.db,
            &state.broadcaster,
            config(),
            peckboard::provider::manager::MidTurnPolicy::Queue,
            true,
        )
        .await
        .unwrap();

    let mut running = false;
    for _ in 0..200 {
        if state.session_manager.is_running("s1").await {
            running = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(running, "mock:ask run should stay alive waiting on stdin");

    state
        .session_manager
        .send_or_queue(
            "s1",
            "second".into(),
            &state.db,
            &state.broadcaster,
            config(),
            peckboard::provider::manager::MidTurnPolicy::Queue,
            true,
        )
        .await
        .unwrap();
    assert!(
        state.db.next_queued_message("s1").await.unwrap().is_some(),
        "the follow-up must have been queued (mock:ask is per-turn)"
    );
}

async fn post(state: &Arc<AppState>, token: &str, path: &str) -> StatusCode {
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    router(state.clone())
        .with_state(state.clone())
        .oneshot(req)
        .await
        .unwrap()
        .status()
}

async fn assert_stop_is_final(state: &Arc<AppState>, path_status: StatusCode, label: &str) {
    assert_eq!(
        path_status,
        StatusCode::NO_CONTENT,
        "{label} route should return 204"
    );

    tokio::time::sleep(Duration::from_millis(400)).await;

    assert!(
        !state.session_manager.is_running("s1").await,
        "{label}: session must NOT be running afterwards (it respawned from the queue)"
    );
    assert!(
        state.db.next_queued_message("s1").await.unwrap().is_none(),
        "{label}: the queued follow-up must be cleared by an explicit stop"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn terminate_clears_queue_and_does_not_respawn() {
    let (state, token) = build_state().await;
    send_and_queue_followup(&state).await;

    let status = post(&state, &token, "/api/sessions/s1/terminate").await;
    assert_stop_is_final(&state, status, "terminate").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupt_drains_queued_followup_into_fresh_run() {
    let (state, token) = build_state().await;
    send_and_queue_followup(&state).await;

    let status = post(&state, &token, "/api/sessions/s1/interrupt").await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "interrupt route should return 204"
    );

    let mut drained_into_run = false;
    for _ in 0..300 {
        let queue_empty = state.db.next_queued_message("s1").await.unwrap().is_none();
        let running = state.session_manager.is_running("s1").await;
        if queue_empty && running {
            drained_into_run = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        drained_into_run,
        "interrupt must drain the queued follow-up into a fresh run (release-and-continue)"
    );
}
