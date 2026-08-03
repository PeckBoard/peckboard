//! Regression test for the ask_user respawn loop: a worker that asks the
//! user a question must stay parked on that card until the answer lands.
//!
//! Before the fix, `ask_user` wrote only events — no `card.blocked` — and
//! the watchdog's stale-ref sweep cleared `worker_session_id` once the idle
//! session aged past `ORPHAN_GRACE_SECS` (90s). The card was then
//! unassigned, unblocked, non-terminal and deps-satisfied, so
//! `check_and_spawn_workers_at` respawned it — a fresh billed agent run and
//! a duplicate question every couple of minutes, uncapped (the ask-user
//! path appends no `NO_PROGRESS_KIND` event, so the no-progress backoff
//! never engages).
//!
//! Covers both halves of the fix: the card block
//! (`questions::ASK_USER_BLOCK_REASON`) and the watchdog's
//! pending-question skip.

use std::sync::Arc;
use std::time::Duration;

use peckboard::auth::rate_limit::RateLimiter;
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewCard, NewFolder, NewProject, UpdateSession};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::mock::register_mock_provider;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::service::mcp_server::{McpTokenRegistry, McpToolRegistry, ToolCallContext};
use peckboard::service::push::PushService;
use peckboard::service::questions::{ASK_USER_BLOCK_REASON, resolve_question};
use peckboard::state::AppState;
use peckboard::worker::orchestrator::{check_and_spawn_workers_at, handle_worker_done};
use peckboard::worker::watchdog::sweep_stale_card_refs;
use peckboard::ws::broadcaster::Broadcaster;

async fn build_state() -> Arc<AppState> {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();
    // Keep the dir alive for the whole test process; same trick the other
    // orchestrator integration tests use.
    std::mem::forget(tmp);

    let registry = Arc::new(ProviderRegistry::new());
    register_mock_provider(&registry).await;

    let db = Db::in_memory().unwrap();
    let plugins = Arc::new(PluginManager::new(&data_dir, db.clone()));
    let session_manager = SessionManager::new(registry.clone()).with_plugins(plugins.clone());

    Arc::new(AppState {
        env_unlock: Arc::new(peckboard::service::env_vars::EnvUnlockRegistry::new()),
        config: Config {
            port: 0,
            https_port: 0,
            host: "127.0.0.1".into(),
            data_dir: data_dir.clone(),
            mdns: false,
            keep_alive_hours: 0,
            provider_send_timeout_secs: 300,
        },
        db,
        plugins,
        builtin_plugins: Arc::new(BuiltinPluginRegistry::new()),
        jwt_secret: vec![0u8; 32],
        login_limiter: RateLimiter::new(100),
        password_change_limiter: RateLimiter::new(100),
        broadcaster: Broadcaster::new(),
        provider_registry: registry,
        session_manager,
        repeating_task_manager: peckboard::repeating::RepeatingTaskManager::new(),
        run_auditor: peckboard::repeating::RunAuditor::new(),
        mcp_tokens: McpTokenRegistry::new(),
        tls: Arc::new(peckboard::state::TlsState::new()),
        push_service: PushService::new(&data_dir),
    })
}

/// Folder + active single-worker project on `mock:happy-path` + one card.
async fn seed_project_and_card(state: &Arc<AppState>) {
    let ts = chrono::Utc::now().to_rfc3339();
    state
        .db
        .create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp".into(),
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
            model: Some("mock:happy-path".into()),
            effort: None,
            budget_usd_cents: None,
            budget_period: None,
            worktree_isolation: false,
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
            title: "needs a decision".into(),
            description: "".into(),
            step: "backlog".into(),
            priority: 1,
            workflow: "task".into(),
            model: None,
            effort: None,
            blocked: false,
            block_reason: None,
            created_at: ts.clone(),
            updated_at: ts,
            system_prompt_name: None,
        })
        .await
        .unwrap();
}

async fn worker_session_ids(state: &Arc<AppState>) -> Vec<String> {
    state
        .db
        .list_worker_sessions_by_project("p1")
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect()
}

#[tokio::test]
async fn ask_user_parks_card_until_answered_then_resumes_once() {
    let state = build_state().await;
    seed_project_and_card(&state).await;

    let mut completion_rx = state
        .session_manager
        .take_completion_rx()
        .await
        .expect("completion rx available");

    let now = chrono::Utc::now();
    check_and_spawn_workers_at(&state, now).await;

    let card = state.db.get_card("c1").await.unwrap().unwrap();
    let session_id = card
        .worker_session_id
        .clone()
        .expect("orchestrator must spawn a worker for the card");

    let completion = tokio::time::timeout(Duration::from_secs(5), completion_rx.recv())
        .await
        .expect("mock turn must complete")
        .expect("completion channel open");
    assert_eq!(completion.session_id, session_id);

    // The worker asks the user, through the real MCP tool.
    let tools = McpToolRegistry::new();
    let ctx = ToolCallContext {
        session_id: session_id.clone(),
        project_id: Some("p1".into()),
        card_id: Some("c1".into()),
        db: Arc::new(state.db.clone()),
        broadcaster: state.broadcaster.clone(),
        provider_registry: None,
        data_dir: None,
        folder_id: "f1".into(),
    };
    tools
        .handle_tool_call(
            "ask_user",
            serde_json::json!({
                "questions": [{ "question": "Which database?", "header": "Setup" }]
            }),
            &ctx,
        )
        .await
        .unwrap();
    handle_worker_done(&state, &session_id).await;

    let card = state.db.get_card("c1").await.unwrap().unwrap();
    assert!(card.blocked, "asking the user must block the card");
    assert_eq!(card.block_reason.as_deref(), Some(ASK_USER_BLOCK_REASON));
    assert_eq!(
        card.worker_session_id.as_deref(),
        Some(session_id.as_str()),
        "the card stays assigned to the asking worker"
    );

    // Age the session well past ORPHAN_GRACE_SECS with the question still
    // unanswered, then run the real watchdog sweep + an orchestrator pass
    // an hour into the future: neither may hand the card back out.
    state
        .db
        .update_session(
            &session_id,
            UpdateSession {
                last_activity: Some(
                    (chrono::Utc::now() - chrono::Duration::seconds(600)).to_rfc3339(),
                ),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    sweep_stale_card_refs(&state.db, &state.session_manager).await;

    let card = state.db.get_card("c1").await.unwrap().unwrap();
    assert_eq!(
        card.worker_session_id.as_deref(),
        Some(session_id.as_str()),
        "the watchdog must not unassign a card awaiting a user answer"
    );

    check_and_spawn_workers_at(&state, now + chrono::Duration::seconds(3600)).await;
    assert_eq!(
        worker_session_ids(&state).await,
        vec![session_id.clone()],
        "no respawn while the question is unanswered"
    );
    assert!(
        state.db.get_card("c1").await.unwrap().unwrap().blocked,
        "card stays blocked until the question is answered"
    );

    // The user answers: the card unblocks and the SAME session resumes.
    let question_id = state
        .db
        .list_events_by_session(&session_id, None)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "question")
        .expect("ask_user must append a question event")
        .id;
    resolve_question(
        state.clone(),
        "u1".into(),
        session_id.clone(),
        serde_json::json!({
            "question_id": question_id,
            "answers": { "0": "SQLite" }
        }),
    )
    .await
    .unwrap();

    let card = state.db.get_card("c1").await.unwrap().unwrap();
    assert!(!card.blocked, "answering must unblock the card");
    assert_eq!(card.block_reason, None);

    // Exactly one resume: the answer goes to the original session, and no
    // second worker is spawned for the card.
    let completion = tokio::time::timeout(Duration::from_secs(5), completion_rx.recv())
        .await
        .expect("the answered worker must resume")
        .expect("completion channel open");
    assert_eq!(
        completion.session_id, session_id,
        "the answer resumes the asking session, not a fresh worker"
    );

    check_and_spawn_workers_at(&state, now + chrono::Duration::seconds(7200)).await;
    assert_eq!(
        worker_session_ids(&state).await,
        vec![session_id],
        "answering must not spawn a duplicate worker"
    );
}

/// A worker whose question was already answered keeps working and ends its
/// turn as an ordinary "Continue" (no `complete_step`/`finish_card`, so no
/// `step-change` event). `derive_worker_intent` still sees the old
/// `ask-user-requested` in its window; `handle_worker_done` must not act on
/// it, or the card is re-blocked on a question nobody can answer.
#[tokio::test]
async fn answered_question_does_not_reblock_card_on_plain_turn_end() {
    let state = build_state().await;
    seed_project_and_card(&state).await;

    let mut completion_rx = state
        .session_manager
        .take_completion_rx()
        .await
        .expect("completion rx available");

    let now = chrono::Utc::now();
    check_and_spawn_workers_at(&state, now).await;

    let card = state.db.get_card("c1").await.unwrap().unwrap();
    let session_id = card
        .worker_session_id
        .clone()
        .expect("orchestrator must spawn a worker for the card");

    tokio::time::timeout(Duration::from_secs(5), completion_rx.recv())
        .await
        .expect("mock turn must complete")
        .expect("completion channel open");

    let tools = McpToolRegistry::new();
    let ctx = ToolCallContext {
        session_id: session_id.clone(),
        project_id: Some("p1".into()),
        card_id: Some("c1".into()),
        db: Arc::new(state.db.clone()),
        broadcaster: state.broadcaster.clone(),
        provider_registry: None,
        data_dir: None,
        folder_id: "f1".into(),
    };
    tools
        .handle_tool_call(
            "ask_user",
            serde_json::json!({
                "questions": [{ "question": "Which database?", "header": "Setup" }]
            }),
            &ctx,
        )
        .await
        .unwrap();
    handle_worker_done(&state, &session_id).await;
    assert!(state.db.get_card("c1").await.unwrap().unwrap().blocked);

    let question_id = state
        .db
        .list_events_by_session(&session_id, None)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "question")
        .expect("ask_user must append a question event")
        .id;
    resolve_question(
        state.clone(),
        "u1".into(),
        session_id.clone(),
        serde_json::json!({
            "question_id": question_id,
            "answers": { "0": "SQLite" }
        }),
    )
    .await
    .unwrap();

    tokio::time::timeout(Duration::from_secs(5), completion_rx.recv())
        .await
        .expect("the answered worker must resume")
        .expect("completion channel open");

    let card = state.db.get_card("c1").await.unwrap().unwrap();
    assert!(!card.blocked, "answering must unblock the card");

    // The resumed turn ends without advancing the card: plain Continue.
    handle_worker_done(&state, &session_id).await;

    let card = state.db.get_card("c1").await.unwrap().unwrap();
    assert!(
        !card.blocked,
        "a resolved question must not re-block the card: {:?}",
        card.block_reason
    );
    assert_eq!(card.block_reason, None);
    assert_eq!(
        card.worker_session_id, None,
        "the Continue path clears the assignment"
    );
    assert_eq!(
        card.last_worker_session_id.as_deref(),
        Some(session_id.as_str()),
        "the Continue path stamps last_worker_session_id so the session resumes"
    );
}
