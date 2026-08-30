//! Integration coverage for recovery-mode account/provider switch:
//!
//! When the outgoing agent cannot write a handover doc (usage limit), the
//! user can send the reconstructed transcript to the incoming model
//! instead. These tests pin:
//!
//! - GET /recovery-preview returns token count + input cost
//! - POST /recover flips the model immediately (no outgoing agent, no
//!   `handover_to_model` park) and injects the transcript on the next turn
//! - the same 409/400 guards as a summary handover (worker, mid-turn,
//!   same continuity key, empty history)

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewAuthSession, NewFolder, NewSession, NewUser};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::message::UserMessage;
use peckboard::provider::mock::register_mock_provider;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::provider::stream::SpawnConfig;
use peckboard::routes::sessions::router;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::ws::broadcaster::Broadcaster;
use tower::ServiceExt;

async fn build_state(session_model: &str) -> (Arc<AppState>, String) {
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
        model: Some(session_model.into()),
        created_at: ts.clone(),
        last_activity: ts,
        ..Default::default()
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
    (state, token)
}

async fn seed_history(db: &Db, closed: bool) {
    db.append_event(
        "s1",
        "user",
        serde_json::json!({ "text": "please fix foo.rs" }),
    )
    .await
    .unwrap();
    db.append_event("s1", "agent-start", serde_json::json!({ "model": "m" }))
        .await
        .unwrap();
    db.append_event(
        "s1",
        "agent-text",
        serde_json::json!({ "text": "I will look at foo.rs." }),
    )
    .await
    .unwrap();
    if closed {
        db.append_event(
            "s1",
            "agent-end",
            serde_json::json!({ "status": "complete" }),
        )
        .await
        .unwrap();
    }
}

async fn seed_worker(state: &AppState) {
    let ts = chrono::Utc::now().to_rfc3339();
    state
        .db
        .create_session(NewSession {
            id: "w1".into(),
            name: "W".into(),
            folder_id: "f1".into(),
            model: Some("mock:echo".into()),
            is_worker: true,
            created_at: ts.clone(),
            last_activity: ts,
            ..Default::default()
        })
        .await
        .unwrap();
}

async fn oneshot(state: &Arc<AppState>, req: Request<Body>) -> (StatusCode, serde_json::Value) {
    let resp = router(state.clone())
        .with_state(state.clone())
        .oneshot(req)
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
    (status, body)
}

async fn preview(
    state: &Arc<AppState>,
    token: &str,
    model: &str,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("GET")
        .uri(format!(
            "/api/sessions/s1/recovery-preview?model={}",
            urlencoding::encode(model)
        ))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    oneshot(state, req).await
}

async fn recover(
    state: &Arc<AppState>,
    token: &str,
    session_id: &str,
    model: &str,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/sessions/{session_id}/recover"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(format!(r#"{{"model":"{model}"}}"#)))
        .unwrap();
    oneshot(state, req).await
}

#[tokio::test]
async fn preview_returns_token_count_and_cost() {
    let (state, token) = build_state("mock:echo").await;
    seed_history(&state.db, true).await;

    let (status, body) = preview(&state, &token, "mock:echo@acct2").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let tokens = body["tokens"].as_i64().expect("tokens");
    assert!(tokens > 0, "preview must count the transcript: {body}");
    assert!(
        body["est_cost_usd"].as_f64().unwrap() > 0.0,
        "unknown models price at the conservative Opus input rate: {body}"
    );
    assert_eq!(body["to_model"], "mock:echo@acct2");
    assert_eq!(body["from_model"], "mock:echo");
    assert_eq!(body["fits"], true);
    assert!(body["context_window"].as_i64().unwrap() >= 200_000);
}

#[tokio::test]
async fn recover_flips_model_without_outgoing_agent() {
    let (state, token) = build_state("mock:echo").await;
    seed_history(&state.db, true).await;

    let (status, body) = recover(&state, &token, "s1", "mock:echo@acct2").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["model"], "mock:echo@acct2");
    assert!(
        body["handover_to_model"].is_null(),
        "recovery is immediate — no parked outgoing-agent turn: {body}"
    );
    assert!(
        body["conversation_id"].is_null(),
        "incoming account cannot resume the outgoing conversation"
    );

    let s = state.db.get_session("s1").await.unwrap().unwrap();
    let doc = s
        .pending_handover_doc
        .expect("transcript parked for the next turn");
    assert!(doc.contains("please fix foo.rs"), "doc was: {doc}");
    assert!(doc.contains("I will look at foo.rs."), "doc was: {doc}");

    let events = state.db.events_tail("s1", 50).await.unwrap();
    let handover = events
        .iter()
        .find(|e| e.kind == "handover")
        .expect("recovery records a handover event");
    let data: serde_json::Value = serde_json::from_str(&handover.data).unwrap();
    assert_eq!(data["recovery"], true);
    assert_eq!(data["to"], "mock:echo@acct2");
    assert!(
        !events.iter().any(|e| e.kind == "handover-start"),
        "recovery must not dispatch a doc-generation turn"
    );
}

#[tokio::test]
async fn recover_injects_transcript_on_next_turn() {
    let (state, token) = build_state("mock:echo").await;
    seed_history(&state.db, true).await;

    let (status, _) = recover(&state, &token, "s1", "mock:echo@acct2").await;
    assert_eq!(status, StatusCode::OK);

    let mut completion_rx = state
        .session_manager
        .take_completion_rx()
        .await
        .expect("completion rx available");

    let lock = state.session_manager.lock_session("s1").await;
    state
        .session_manager
        .send_message_locked(
            &lock,
            UserMessage::from_text("continue please"),
            &state.db,
            &state.broadcaster,
            SpawnConfig {
                model: "mock:echo@acct2".into(),
                ..SpawnConfig::default()
            },
        )
        .await
        .expect("mock dispatch succeeds");
    drop(lock);

    tokio::time::timeout(std::time::Duration::from_secs(5), completion_rx.recv())
        .await
        .expect("turn completes")
        .expect("channel open");

    let events = state.db.events_tail("s1", 100).await.unwrap();
    let joined: String = events
        .iter()
        .filter(|e| e.kind == "agent-text")
        .filter_map(|e| {
            serde_json::from_str::<serde_json::Value>(&e.data)
                .ok()
                .and_then(|v| v.get("text")?.as_str().map(str::to_string))
        })
        .collect();
    assert!(
        joined.contains("Recovery context"),
        "incoming model must see the recovery wrapper: {joined}"
    );
    assert!(
        joined.contains("please fix foo.rs"),
        "incoming model must see the original user turn: {joined}"
    );
    assert!(
        joined.contains("continue please"),
        "incoming model must see the new user message: {joined}"
    );

    let s = state.db.get_session("s1").await.unwrap().unwrap();
    assert!(
        s.pending_handover_doc.is_none(),
        "transcript consumed on the first turn"
    );
}

#[tokio::test]
async fn recover_mid_turn_is_refused() {
    let (state, token) = build_state("mock:echo").await;
    seed_history(&state.db, false).await;

    let (status, body) = recover(&state, &token, "s1", "mock:echo@acct2").await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    let s = state.db.get_session("s1").await.unwrap().unwrap();
    assert_eq!(s.model.as_deref(), Some("mock:echo"));
    assert!(s.pending_handover_doc.is_none());
}

#[tokio::test]
async fn recover_worker_is_refused() {
    let (state, token) = build_state("mock:echo").await;
    seed_worker(&state).await;

    let (status, body) = recover(&state, &token, "w1", "mock:echo@acct2").await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
}

#[tokio::test]
async fn recover_same_key_is_refused() {
    let (state, token) = build_state("mock:echo").await;
    seed_history(&state.db, true).await;

    let (status, body) = recover(&state, &token, "s1", "mock:happy-path").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}

#[tokio::test]
async fn recover_empty_history_is_refused() {
    let (state, token) = build_state("mock:echo").await;
    let (status, body) = recover(&state, &token, "s1", "mock:echo@acct2").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}

#[tokio::test]
async fn preview_same_key_is_refused() {
    let (state, token) = build_state("mock:echo").await;
    seed_history(&state.db, true).await;
    let (status, body) = preview(&state, &token, "mock:echo").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}
