//! Integration coverage for the model-switch handover:
//!
//! - the two new session columns round-trip through `update_session`;
//! - the PATCH route's guards (409 while a turn is streaming, 409 while a
//!   handover is already in flight, same-key switches unaffected);
//! - the full begin → doc turn → finalize flow through the real dispatcher
//!   with the mock provider, driving the completion channel the way the
//!   main.rs listener does.
//!
//! The pure decision/extraction logic is unit-tested in `src/handover.rs`;
//! the user-visible flow is also covered by Playwright (web/e2e).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewAuthSession, NewFolder, NewSession, NewUser, UpdateSession};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::mock::register_mock_provider;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::routes::sessions::router;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::ws::broadcaster::Broadcaster;
use tower::ServiceExt;

async fn seed() -> (Db, String) {
    let db = Db::in_memory().unwrap();
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: "f1".into(),
        name: "F".into(),
        path: "/tmp".into(),
        created_at: ts.clone(),
    })
    .await
    .unwrap();
    db.create_session(NewSession {
        id: "s1".into(),
        name: "S".into(),
        folder_id: "f1".into(),
        model: Some("claude:opus".into()),
        effort: None,
        is_worker: false,
        created_at: ts.clone(),
        last_activity: ts,
        ..Default::default()
    })
    .await
    .unwrap();
    (db, "s1".into())
}

#[tokio::test]
async fn handover_columns_default_clear() {
    let (db, sid) = seed().await;
    let s = db.get_session(&sid).await.unwrap().unwrap();
    assert_eq!(s.handover_to_model, None);
    assert_eq!(s.pending_handover_doc, None);
}

#[tokio::test]
async fn handover_columns_round_trip() {
    let (db, sid) = seed().await;

    // Park a target (begin_handover shape) — model deliberately unchanged.
    db.update_session(
        &sid,
        UpdateSession {
            handover_to_model: Some(Some("grok:grok-4".into())),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let s = db.get_session(&sid).await.unwrap().unwrap();
    assert_eq!(s.handover_to_model.as_deref(), Some("grok:grok-4"));
    assert_eq!(s.model.as_deref(), Some("claude:opus"));

    // Finalize shape — flip model, clear the flag, stash the doc.
    db.update_session(
        &sid,
        UpdateSession {
            model: Some(Some("grok:grok-4".into())),
            handover_to_model: Some(None),
            pending_handover_doc: Some(Some("## Goal\ncontinue".into())),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let s = db.get_session(&sid).await.unwrap().unwrap();
    assert_eq!(s.model.as_deref(), Some("grok:grok-4"));
    assert_eq!(s.handover_to_model, None);
    assert_eq!(s.pending_handover_doc.as_deref(), Some("## Goal\ncontinue"));

    // Consume shape — clear the doc after injection.
    db.update_session(
        &sid,
        UpdateSession {
            pending_handover_doc: Some(None),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let s = db.get_session(&sid).await.unwrap().unwrap();
    assert_eq!(s.pending_handover_doc, None);
}

// ── Route-level flow + guards ────────────────────────────────────────

/// Full AppState + auth token for oneshot requests against the sessions
/// router. Mirrors tests/terminate_route.rs. The session "s1" is created
/// on the mock provider so handover doc-generation turns actually run.
async fn build_state(session_model: &str) -> (Arc<AppState>, String) {
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

async fn patch_model(
    state: &Arc<AppState>,
    token: &str,
    model: &str,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("PATCH")
        .uri("/api/sessions/s1")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(format!(r#"{{"model":"{model}"}}"#)))
        .unwrap();
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

/// Seed the event shapes the guards read: prior agent activity, with or
/// without a closing agent-end.
async fn seed_turn(db: &Db, closed: bool) {
    db.append_event("s1", "agent-start", serde_json::json!({ "model": "m" }))
        .await
        .unwrap();
    db.append_event("s1", "agent-text", serde_json::json!({ "text": "hi" }))
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

#[tokio::test]
async fn cross_boundary_switch_mid_turn_is_refused() {
    let (state, token) = build_state("mock:echo").await;
    seed_turn(&state.db, false).await; // agent-start with no agent-end

    let (status, body) = patch_model(&state, &token, "mock:echo@acct2").await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");

    // Nothing changed: no parked target, model untouched.
    let s = state.db.get_session("s1").await.unwrap().unwrap();
    assert_eq!(s.model.as_deref(), Some("mock:echo"));
    assert_eq!(s.handover_to_model, None);
}

#[tokio::test]
async fn same_key_switch_mid_turn_is_allowed() {
    let (state, token) = build_state("mock:echo").await;
    seed_turn(&state.db, false).await;

    // Same provider+account — no handover needed, plain switch goes through
    // even mid-turn (pre-existing behaviour).
    let (status, body) = patch_model(&state, &token, "mock:happy-path").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let s = state.db.get_session("s1").await.unwrap().unwrap();
    assert_eq!(s.model.as_deref(), Some("mock:happy-path"));
    assert_eq!(s.handover_to_model, None);
}

#[tokio::test]
async fn second_switch_during_handover_is_refused() {
    let (state, token) = build_state("mock:echo").await;
    seed_turn(&state.db, true).await;
    state
        .db
        .update_session(
            "s1",
            UpdateSession {
                handover_to_model: Some(Some("mock:echo@acct2".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let (status, body) = patch_model(&state, &token, "mock:echo@acct3").await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    let s = state.db.get_session("s1").await.unwrap().unwrap();
    assert_eq!(s.handover_to_model.as_deref(), Some("mock:echo@acct2"));
}

/// The regression this file exists for: the handover must FINISH. The PATCH
/// parks the target and dispatches the doc turn; the doc turn's completion
/// must arrive on the completion channel (for Claude that only happens
/// because begin_handover requests shutdown_after_turn — per-turn providers
/// deliver it anyway), and finalize must flip the model and stash the doc.
#[tokio::test]
async fn handover_runs_to_completion() {
    let (state, token) = build_state("mock:echo").await;
    seed_turn(&state.db, true).await;

    let mut completion_rx = state
        .session_manager
        .take_completion_rx()
        .await
        .expect("completion rx available");

    let (status, body) = patch_model(&state, &token, "mock:echo@acct2").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    // Parked, not flipped: the doc turn must still route to the outgoing
    // provider/account.
    assert_eq!(body["handover_to_model"], "mock:echo@acct2");
    assert_eq!(body["model"], "mock:echo");

    // The doc-generation turn completes and delivers a ProcessCompletion —
    // this is exactly what the main.rs listener waits on.
    let completion = tokio::time::timeout(std::time::Duration::from_secs(5), completion_rx.recv())
        .await
        .expect("doc turn must deliver a completion (handover would hang forever without it)")
        .expect("channel open");
    assert_eq!(completion.session_id, "s1");

    peckboard::handover::finalize_handover(&state, "s1")
        .await
        .unwrap();

    let s = state.db.get_session("s1").await.unwrap().unwrap();
    assert_eq!(s.model.as_deref(), Some("mock:echo@acct2"));
    assert_eq!(s.handover_to_model, None);
    // mock:echo echoes the doc prompt back, so the captured doc carries it.
    let doc = s.pending_handover_doc.expect("doc stashed for injection");
    assert!(doc.contains("HANDOVER"), "doc was: {doc}");

    // And the durable handover event is on the log.
    let events = state.db.events_tail("s1", 50).await.unwrap();
    assert!(events.iter().any(|e| e.kind == "handover"));
    assert!(events.iter().any(|e| e.kind == "handover-start"));
}
