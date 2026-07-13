//! Integration coverage for the model-switch handover and auto-compaction:
//!
//! - the two new session columns round-trip through `update_session`;
//! - the PATCH route's guards (409 while a turn is streaming, 409 while a
//!   handover is already in flight, same-key switches unaffected);
//! - the full begin → doc turn → finalize flow through the real dispatcher
//!   with the mock provider, driving the completion channel the way the
//!   main.rs listener does;
//! - `maybe_auto_compact`'s threshold/queued/pending-doc guards, the worker
//!   resume-link eligibility, and the worker end-to-end path (compact →
//!   finalize → orchestrator resumes the session with the doc injected).
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
use peckboard::db::models::{
    NewAuthSession, NewCard, NewFolder, NewProject, NewQueuedMessage, NewSession, NewUsageEvent,
    NewUser, UpdateCard, UpdateSession,
};
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
    patch_model_on(state, token, "s1", model).await
}

/// Like [`patch_model`] but against an arbitrary session id — the worker
/// tests target "w1".
async fn patch_model_on(
    state: &Arc<AppState>,
    token: &str,
    session_id: &str,
    model: &str,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("PATCH")
        .uri(format!("/api/sessions/{session_id}"))
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

/// A worker switching within its provider+account is a plain switch: the
/// row updates immediately. (The PATCH handler also hard-cancels any live
/// child so the orchestrator resumes it under the new model — a no-op
/// here, where no child is running.)
#[tokio::test]
async fn worker_same_key_switch_is_applied() {
    let (state, token) = build_state("mock:echo").await;
    seed_worker(&state).await;

    let (status, body) = patch_model_on(&state, &token, "w1", "mock:happy-path").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let s = state.db.get_session("w1").await.unwrap().unwrap();
    assert_eq!(s.model.as_deref(), Some("mock:happy-path"));
    assert_eq!(s.handover_to_model, None);
}

/// A worker switching across the provider/account boundary is refused: no
/// handover doc turn can run mid-card, and a silent plain switch would
/// strand the card's resume on a conversation the incoming provider can't
/// open (crash-looping into auto-pause).
#[tokio::test]
async fn worker_cross_boundary_switch_is_refused() {
    let (state, token) = build_state("mock:echo").await;
    seed_worker(&state).await;

    let (status, body) = patch_model_on(&state, &token, "w1", "mock:echo@acct2").await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    let s = state.db.get_session("w1").await.unwrap().unwrap();
    assert_eq!(s.model.as_deref(), Some("mock:echo"));
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

// ─── Auto-compaction ──────────────────────────────────────────────────────────

/// Record a usage row so `latest_context_tokens` reports `context` occupancy.
async fn seed_context(db: &Db, id: &str, session_id: &str, context: i64) {
    seed_context_at(db, id, session_id, context, 1).await;
}

/// Like [`seed_context`] but with an explicit `ts`, for rows that must land
/// after a `context_reset_ts` stamp.
async fn seed_context_at(db: &Db, id: &str, session_id: &str, context: i64, ts: i64) {
    db.record_usage_event(NewUsageEvent {
        id: id.into(),
        session_id: session_id.into(),
        ts,
        context_tokens: context,
        total_tokens: context,
        model: Some("mock:echo".into()),
        ..Default::default()
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn auto_compact_worker_skips_under_threshold() {
    let (state, _token) = build_state("mock:echo").await;
    seed_worker(&state).await;
    // 150k is over the interactive prompt point but under the worker
    // auto-compaction threshold (200k) — a worker here must not compact.
    seed_context(&state.db, "u1", "w1", 150_000).await;

    let did = peckboard::handover::maybe_auto_compact(&state, "w1")
        .await
        .unwrap();
    assert!(!did);
    let s = state.db.get_session("w1").await.unwrap().unwrap();
    assert_eq!(s.handover_to_model, None);
}

/// Interactive sessions are never auto-compacted, even well past the old
/// threshold — the UI prompts the user (clear / compact / continue). Only
/// workers compact unattended.
#[tokio::test]
async fn auto_compact_skips_interactive_session() {
    let (state, _token) = build_state("mock:echo").await;
    seed_turn(&state.db, true).await;
    seed_context(&state.db, "u1", "s1", 210_000).await;

    let did = peckboard::handover::maybe_auto_compact(&state, "s1")
        .await
        .unwrap();
    assert!(!did, "interactive sessions must never auto-compact");
    let s = state.db.get_session("s1").await.unwrap().unwrap();
    assert_eq!(s.handover_to_model, None);
}

#[tokio::test]
async fn auto_compact_skips_when_message_queued() {
    let (state, _token) = build_state("mock:echo").await;
    seed_worker(&state).await;
    seed_context(&state.db, "u1", "w1", 210_000).await;
    state
        .db
        .upsert_queued_message(NewQueuedMessage {
            session_id: "w1".into(),
            text: "queued while busy".into(),
            queued_at: chrono::Utc::now().to_rfc3339(),
            model: None,
            effort: None,
        })
        .await
        .unwrap();

    let did = peckboard::handover::maybe_auto_compact(&state, "w1")
        .await
        .unwrap();
    assert!(!did, "a queued message must defer compaction");
}

/// Worker over the threshold: the compaction dispatches unprompted, runs to
/// completion, and a second check right after the finalize is a no-op (the
/// pending-doc guard).
#[tokio::test]
async fn auto_compact_dispatches_for_worker_over_threshold() {
    let (state, _token) = build_state("mock:echo").await;
    seed_worker(&state).await;
    seed_context(&state.db, "u1", "w1", 210_000).await;

    let mut completion_rx = state
        .session_manager
        .take_completion_rx()
        .await
        .expect("completion rx available");

    let did = peckboard::handover::maybe_auto_compact(&state, "w1")
        .await
        .unwrap();
    assert!(did);

    // Same-model handover parked; the start marker is flagged compaction.
    let s = state.db.get_session("w1").await.unwrap().unwrap();
    assert_eq!(s.handover_to_model.as_deref(), Some("mock:echo"));
    let events = state.db.events_tail("w1", 50).await.unwrap();
    let start = events
        .iter()
        .find(|e| e.kind == "handover-start")
        .expect("start marker");
    let data: serde_json::Value = serde_json::from_str(&start.data).unwrap();
    assert_eq!(data["compaction"], true);

    // Doc turn completes; finalize leaves an idle session carrying the doc.
    tokio::time::timeout(std::time::Duration::from_secs(5), completion_rx.recv())
        .await
        .expect("doc turn must complete")
        .expect("channel open");
    peckboard::handover::finalize_handover(&state, "w1")
        .await
        .unwrap();
    let s = state.db.get_session("w1").await.unwrap().unwrap();
    assert_eq!(s.handover_to_model, None);
    assert_eq!(s.conversation_id, None);
    assert!(s.pending_handover_doc.is_some());

    // Occupancy reads 0 (finalize stamps `context_reset_ts`), and the
    // pending doc must block a re-compaction regardless.
    let again = peckboard::handover::maybe_auto_compact(&state, "w1")
        .await
        .unwrap();
    assert!(!again, "pending doc must block a second compaction");
}

/// Finalizing a compaction resets the reported context occupancy: the
/// pre-compaction rows (and the doc turn's own full-context row) belong to
/// the discarded conversation, so the badge/threshold must read 0 until the
/// fresh conversation's first turn records usage.
#[tokio::test]
async fn finalize_resets_context_occupancy() {
    let (state, _token) = build_state("mock:echo").await;
    seed_worker(&state).await;
    seed_context(&state.db, "u1", "w1", 210_000).await;
    assert_eq!(
        state.db.latest_context_tokens("w1").await.unwrap(),
        Some(210_000)
    );

    let mut completion_rx = state
        .session_manager
        .take_completion_rx()
        .await
        .expect("completion rx available");
    assert!(
        peckboard::handover::maybe_auto_compact(&state, "w1")
            .await
            .unwrap()
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), completion_rx.recv())
        .await
        .expect("doc turn must complete")
        .expect("channel open");
    peckboard::handover::finalize_handover(&state, "w1")
        .await
        .unwrap();

    assert_eq!(
        state.db.latest_context_tokens("w1").await.unwrap(),
        None,
        "post-compaction occupancy must not report the discarded conversation",
    );

    // A turn recorded after the reset is reported again.
    let s = state.db.get_session("w1").await.unwrap().unwrap();
    let reset = s
        .context_reset_ts
        .expect("finalize must stamp context_reset_ts");
    seed_context_at(&state.db, "u2", "w1", 12_000, reset + 1).await;
    assert_eq!(
        state.db.latest_context_tokens("w1").await.unwrap(),
        Some(12_000)
    );
}

/// Worker fixture: project p1 / card c1 on step "implement", worker session
/// w1 that already ran a chunk (conversation to resume, card unclaimed with
/// the resume link pointing at w1) — the post-`handle_worker_done` Continue
/// state the listener sees when it runs the compaction check.
async fn seed_worker(state: &Arc<AppState>) {
    let ts = chrono::Utc::now().to_rfc3339();
    state
        .db
        .create_project(NewProject {
            id: "p1".into(),
            name: "P".into(),
            context: "ctx".into(),
            folder_id: "f1".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "kanban".into(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: true,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts.clone(),
            budget_usd_cents: None,
            budget_period: None,
            worktree_isolation: false,
        })
        .await
        .unwrap();
    state
        .db
        .create_card(NewCard {
            id: "c1".into(),
            project_id: "p1".into(),
            title: "card".into(),
            description: "desc".into(),
            step: "implement".into(),
            priority: 1,
            workflow: "kanban".into(),
            model: None,
            effort: None,
            blocked: false,
            block_reason: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
            system_prompt_name: None,
        })
        .await
        .unwrap();
    state
        .db
        .create_session(NewSession {
            id: "w1".into(),
            name: "worker: card".into(),
            folder_id: "f1".into(),
            model: Some("mock:echo".into()),
            is_worker: true,
            project_id: Some("p1".into()),
            card_id: Some("c1".into()),
            conversation_id: Some("conv-1".into()),
            worker_step: Some("implement".into()),
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    state
        .db
        .update_card(
            "c1",
            UpdateCard {
                worker_session_id: Some(None),
                last_worker_session_id: Some(Some("w1".into())),
                updated_at: Some(ts),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    state
        .db
        .append_event(
            "w1",
            "agent-text",
            serde_json::json!({ "text": "chunk work" }),
        )
        .await
        .unwrap();
}

/// A worker only compacts while its card would still resume THIS session;
/// every broken link variant must skip.
#[tokio::test]
async fn auto_compact_worker_requires_resume_link() {
    let (state, _token) = build_state("mock:echo").await;
    seed_worker(&state).await;
    seed_context(&state.db, "u1", "w1", 210_000).await;

    let ts = chrono::Utc::now().to_rfc3339();

    // Card advanced past the worker's step — a fresh session runs next.
    state
        .db
        .update_card(
            "c1",
            UpdateCard {
                step: Some("review".into()),
                updated_at: Some(ts.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        !peckboard::handover::maybe_auto_compact(&state, "w1")
            .await
            .unwrap()
    );

    // Card reclaimed by a different session (a real row — the column has a
    // sessions FK).
    state
        .db
        .create_session(NewSession {
            id: "w2".into(),
            name: "worker: replacement".into(),
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
    state
        .db
        .update_card(
            "c1",
            UpdateCard {
                step: Some("implement".into()),
                worker_session_id: Some(Some("w2".into())),
                updated_at: Some(ts.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        !peckboard::handover::maybe_auto_compact(&state, "w1")
            .await
            .unwrap()
    );

    // No conversation to resume for the doc turn.
    state
        .db
        .update_card(
            "c1",
            UpdateCard {
                worker_session_id: Some(None),
                updated_at: Some(ts),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    state
        .db
        .update_session(
            "w1",
            UpdateSession {
                conversation_id: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        !peckboard::handover::maybe_auto_compact(&state, "w1")
            .await
            .unwrap()
    );
    let s = state.db.get_session("w1").await.unwrap().unwrap();
    assert_eq!(s.handover_to_model, None, "no doc turn may have dispatched");

    // Link restored → eligible. The step moves above severed the resume
    // link (`sever_worker_resume_link` nulls the session's `worker_step` on
    // real step changes), so restore that too — the state a Continue
    // completion leaves behind.
    state
        .db
        .update_card(
            "c1",
            UpdateCard {
                last_worker_session_id: Some(Some("w1".into())),
                updated_at: Some(chrono::Utc::now().to_rfc3339()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    state
        .db
        .update_session(
            "w1",
            UpdateSession {
                conversation_id: Some(Some("conv-1".into())),
                worker_step: Some(Some("implement".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        peckboard::handover::maybe_auto_compact(&state, "w1")
            .await
            .unwrap()
    );
}

/// The worker continuity path end to end: auto-compact between chunks →
/// doc turn completes → finalize → the orchestrator's next tick RESUMES the
/// same session (pending doc stands in for the cleared conversation_id) and
/// the dispatch consumes the doc.
#[tokio::test]
async fn worker_auto_compaction_resumes_with_doc() {
    let (state, _token) = build_state("mock:echo").await;
    seed_worker(&state).await;
    seed_context(&state.db, "u1", "w1", 210_000).await;

    let mut completion_rx = state
        .session_manager
        .take_completion_rx()
        .await
        .expect("completion rx available");

    assert!(
        peckboard::handover::maybe_auto_compact(&state, "w1")
            .await
            .unwrap()
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), completion_rx.recv())
        .await
        .expect("doc turn must complete")
        .expect("channel open");
    peckboard::handover::finalize_handover(&state, "w1")
        .await
        .unwrap();

    let s = state.db.get_session("w1").await.unwrap().unwrap();
    assert_eq!(s.conversation_id, None);
    assert!(s.pending_handover_doc.is_some());

    // The orchestrator tick: the relaxed resume filter must pick w1 back up
    // (conversation_id is gone but the doc is parked), not mint a fresh
    // session that would orphan the doc.
    peckboard::worker::orchestrator::check_and_spawn_workers(&state).await;

    let card = state.db.get_card("c1").await.unwrap().unwrap();
    assert_eq!(
        card.worker_session_id.as_deref(),
        Some("w1"),
        "card must resume the compacted session"
    );
    let s = state.db.get_session("w1").await.unwrap().unwrap();
    assert_eq!(
        s.pending_handover_doc, None,
        "the resume dispatch must consume the doc"
    );
}

// ─── Handover abort (don't switch if the switch fails / is interrupted) ──────

/// A failed or interrupted doc-generation turn must NOT switch the model:
/// aborting clears only the parked target and leaves `model` and
/// `conversation_id` intact so no context is lost.
#[tokio::test]
async fn abort_handover_keeps_model_and_context() {
    let (state, _token) = build_state("mock:echo").await;
    // Park a handover and give the session a live conversation to preserve.
    state
        .db
        .update_session(
            "s1",
            UpdateSession {
                conversation_id: Some(Some("conv-x".into())),
                handover_to_model: Some(Some("grok:grok-4@acct2".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    peckboard::handover::abort_handover(
        &state,
        "s1",
        Some("Failed to authenticate. API Error: 401 Invalid authentication credentials"),
    )
    .await
    .unwrap();

    let s = state.db.get_session("s1").await.unwrap().unwrap();
    assert_eq!(s.handover_to_model, None, "parked target cleared");
    assert_eq!(s.model.as_deref(), Some("mock:echo"), "model unchanged");
    assert_eq!(
        s.conversation_id.as_deref(),
        Some("conv-x"),
        "conversation preserved so context isn't lost"
    );
    let events = state.db.events_tail("s1", 50).await.unwrap();
    let aborted = events
        .iter()
        .find(|e| e.kind == "handover-aborted")
        .expect("an abort marker is recorded");
    let data: serde_json::Value = serde_json::from_str(&aborted.data).unwrap();
    assert!(
        data["reason"].as_str().unwrap_or_default().contains("401"),
        "the abort must record why the doc turn failed, got {data}"
    );
}

/// Abort is a no-op when nothing is parked — a spurious completion can't
/// misfire and stamp an abort marker on an ordinary session.
#[tokio::test]
async fn abort_handover_noop_without_parked_target() {
    let (state, _token) = build_state("mock:echo").await;

    peckboard::handover::abort_handover(&state, "s1", None)
        .await
        .unwrap();

    let s = state.db.get_session("s1").await.unwrap().unwrap();
    assert_eq!(s.model.as_deref(), Some("mock:echo"));
    let events = state.db.events_tail("s1", 50).await.unwrap();
    assert!(!events.iter().any(|e| e.kind == "handover-aborted"));
}

/// A compaction whose doc turn produced no text must ABORT, not finalize:
/// there is nothing to inject, so dropping the conversation would destroy
/// the context the compaction was meant to preserve. (Model switches keep
/// the empty-doc fallback — there the user explicitly asked to switch —
/// but a compaction with no doc is pure loss.)
#[tokio::test]
async fn finalize_compaction_with_empty_doc_aborts() {
    let (state, _token) = build_state("mock:echo").await;
    // Park a same-model handover (a compaction) with a live conversation,
    // but record NO agent-text — the failed doc turn's shape.
    state
        .db
        .update_session(
            "s1",
            UpdateSession {
                conversation_id: Some(Some("conv-keep".into())),
                handover_to_model: Some(Some("mock:echo".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    peckboard::handover::finalize_handover(&state, "s1")
        .await
        .unwrap();

    let s = state.db.get_session("s1").await.unwrap().unwrap();
    assert_eq!(s.handover_to_model, None, "parked target cleared");
    assert_eq!(
        s.conversation_id.as_deref(),
        Some("conv-keep"),
        "conversation preserved — no context lost"
    );
    assert_eq!(s.pending_handover_doc, None, "no doc stashed");
    let events = state.db.events_tail("s1", 50).await.unwrap();
    assert!(
        events.iter().any(|e| e.kind == "handover-aborted"),
        "the failed compaction must record an abort, not a handover"
    );
    assert!(
        !events.iter().any(|e| e.kind == "handover"),
        "no 'Context compacted' marker for a failed compaction"
    );
}
