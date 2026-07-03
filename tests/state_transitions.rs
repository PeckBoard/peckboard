//! Durability + race-condition tests for project state transitions and the
//! worker watchdog: card moves, pauses, resumes, and the stale-completion
//! guards in `handle_worker_done` + the completion listener.
//!
//! These exercise the public `worker::orchestrator` + `Db` primitives
//! directly. The MCP / HTTP layers are covered in their own files. No
//! `claude` CLI is involved; the mock provider supplies the
//! `provider_registry` so cancel/is_running calls round-trip the same
//! `AgentProvider` trait the production code uses.

use std::sync::Arc;

use peckboard::auth::rate_limit::RateLimiter;
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{
    NewCard, NewFolder, NewProject, NewQueuedMessage, NewSession, UpdateCard, UpdateProject,
};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::mock::register_mock_provider;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::worker::orchestrator::{
    cancel_worker_for_card_move, check_and_spawn_workers, drain_queue_for_session,
    handle_worker_done,
};
use peckboard::ws::broadcaster::Broadcaster;

async fn build_state() -> Arc<AppState> {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();
    // Keep the dir alive for the whole test process; tests don't share it
    // and the OS will reap on exit. Same trick mcp_card_transitions uses.
    std::mem::forget(tmp);

    let registry = Arc::new(ProviderRegistry::new());
    register_mock_provider(&registry).await;

    let db = Db::in_memory().unwrap();
    let plugins = Arc::new(PluginManager::new(&data_dir, db.clone()));
    let session_manager = SessionManager::new(registry.clone()).with_plugins(plugins.clone());

    Arc::new(AppState {
        config: Config {
            port: 0,
            https_port: 0,
            host: "127.0.0.1".into(),
            data_dir: data_dir.clone(),
            mdns: false,
            keep_alive_hours: 0,
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
        push_service: PushService::new(&data_dir),
    })
}

async fn seed_project(state: &Arc<AppState>) {
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
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: true,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts,
        })
        .await
        .unwrap();
}

async fn seed_card_with_worker(state: &Arc<AppState>, card_id: &str, step: &str, session_id: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    state
        .db
        .create_card(NewCard {
            id: card_id.into(),
            project_id: "p1".into(),
            title: format!("card-{card_id}"),
            description: "".into(),
            step: step.into(),
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
    seed_worker_session(state, session_id, Some(card_id)).await;
    state
        .db
        .update_card(
            card_id,
            UpdateCard {
                worker_session_id: Some(Some(session_id.into())),
                last_worker_session_id: Some(Some(session_id.into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
}

/// Insert a worker session row. Useful when a test needs a session id
/// to be a valid FK target (e.g. modelling the orchestrator spawning a
/// replacement worker on the same card).
async fn seed_worker_session(state: &Arc<AppState>, session_id: &str, card_id: Option<&str>) {
    let ts = chrono::Utc::now().to_rfc3339();
    state
        .db
        .create_session(NewSession {
            id: session_id.into(),
            name: format!("worker-{session_id}"),
            folder_id: "f1".into(),
            model: None,
            effort: None,
            is_worker: true,
            project_id: Some("p1".into()),
            card_id: card_id.map(Into::into),
            conversation_id: None,
            created_at: ts.clone(),
            last_activity: ts,
            ..Default::default()
        })
        .await
        .unwrap();
}

// ── stale-completion guard ──────────────────────────────────────────────

/// Sequence the race that was clobbering replacement workers before the
/// conditional-clear landed:
///
///   1. User drags card `c1` from `in_progress` to `review` — the route
///      handler atomically advances the step AND clears the old worker
///      ref, capturing the old session id for a later cancel.
///   2. Orchestrator's 5s tick (modelled here as a direct DB write)
///      spawns a replacement worker for `review` — card now points at
///      `new-ws`.
///   3. The cancel from step (1) finally lands and the completion
///      listener fires for `old-ws`.
///
/// The old listener path called `update_card(card_id, worker_session_id =
/// None)` unconditionally, which wiped the new assignment and opened the
/// door to a second concurrent worker on the same card.
/// `clear_card_worker_if_matches` makes (3) a no-op.
#[tokio::test]
async fn stale_completion_does_not_clobber_replacement_worker() {
    let state = build_state().await;
    seed_project(&state).await;
    seed_card_with_worker(&state, "c1", "in_progress", "old-ws").await;

    // (1) user-move + atomic clear of the old worker ref.
    state
        .db
        .update_card(
            "c1",
            UpdateCard {
                step: Some("review".into()),
                worker_session_id: Some(None),
                last_worker_session_id: Some(Some("old-ws".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // (2) orchestrator respawn: a new worker takes the slot. The
    // session has to exist for the FK to be satisfied — same as
    // production where `spawn_worker_for_card` inserts the session row
    // before assigning it to the card.
    seed_worker_session(&state, "new-ws", Some("c1")).await;
    state
        .db
        .update_card(
            "c1",
            UpdateCard {
                worker_session_id: Some(Some("new-ws".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // (3) the old worker's cancel completion finally fires.
    let result = state
        .db
        .clear_card_worker_if_matches("c1", "old-ws")
        .await
        .unwrap();
    assert!(
        result.is_none(),
        "stale completion must NOT clear new worker"
    );
    let card = state.db.get_card("c1").await.unwrap().unwrap();
    assert_eq!(
        card.worker_session_id.as_deref(),
        Some("new-ws"),
        "replacement worker still owns the slot"
    );
}

/// `handle_worker_done` runs after a worker process exits gracefully and
/// derives an intent. If the card has already been reassigned (user
/// moved + orchestrator respawned), acting on the stale intent would
/// either clobber the new worker's ref or re-advance the new step. The
/// guard at the top of `handle_worker_done` skips the intent block
/// entirely; the resource-cleanup tail still runs.
#[tokio::test]
async fn handle_worker_done_skips_intent_after_card_reassigned() {
    let state = build_state().await;
    seed_project(&state).await;
    seed_card_with_worker(&state, "c1", "in_progress", "old-ws").await;

    // User moved the card and the orchestrator already gave it a new
    // worker, mirroring the (1)-(2) sequence above. Seed the new
    // session row before the FK-bearing card update.
    seed_worker_session(&state, "new-ws", Some("c1")).await;
    state
        .db
        .update_card(
            "c1",
            UpdateCard {
                step: Some("review".into()),
                worker_session_id: Some(Some("new-ws".into())),
                last_worker_session_id: Some(Some("old-ws".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    handle_worker_done(&state, "old-ws").await;

    let card = state.db.get_card("c1").await.unwrap().unwrap();
    // No step advance, no clobbered ref.
    assert_eq!(card.step, "review");
    assert_eq!(card.worker_session_id.as_deref(), Some("new-ws"));
}

// ── pause kills queued messages + drain is gated ────────────────────────

/// `drain_queue_for_session` runs from the completion listener for every
/// session on every completion path, including the synthetic "Crashed
/// (interrupted)" event that fires when project pause cancels a worker.
/// If the worker had a queued message, the old code would happily drain
/// it into a fresh agent run — defeating the whole point of "stop the
/// project". The new gate refuses to drain when the owning project is
/// paused AND drops the queued row so a later resume can't re-trigger
/// the same branch with a stale message.
#[tokio::test]
async fn drain_queue_skips_paused_project_and_drops_message() {
    let state = build_state().await;
    seed_project(&state).await;
    seed_card_with_worker(&state, "c1", "in_progress", "ws1").await;

    // The worker has a queued message — would have been delivered as a
    // fresh run on the next completion in the unpaused world.
    let ts = chrono::Utc::now().to_rfc3339();
    state
        .db
        .upsert_queued_message(NewQueuedMessage {
            session_id: "ws1".into(),
            text: "do this next".into(),
            queued_at: ts,
            ..Default::default()
        })
        .await
        .unwrap();

    state
        .db
        .update_project(
            "p1",
            UpdateProject {
                status: Some("paused".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    drain_queue_for_session(&state, "ws1").await.unwrap();

    // No new agent run: queue dropped so the next listener pass can't
    // resurrect it either.
    assert!(state.db.get_queued_message("ws1").await.unwrap().is_none());
}

/// Active projects still drain normally — the pause gate must not turn
/// into a generic short-circuit. A queued message on a non-paused
/// project should still be removed from the queue and delivered as a
/// fresh user event when the session is idle.
#[tokio::test]
async fn drain_queue_proceeds_for_active_project() {
    let state = build_state().await;
    seed_project(&state).await;
    seed_card_with_worker(&state, "c1", "in_progress", "ws1").await;

    let ts = chrono::Utc::now().to_rfc3339();
    state
        .db
        .upsert_queued_message(NewQueuedMessage {
            session_id: "ws1".into(),
            text: "hello".into(),
            // Mock model so the dispatch is deterministic and doesn't
            // require the real `claude` CLI.
            queued_at: ts,
            model: Some("mock:echo".into()),
            effort: None,
        })
        .await
        .unwrap();

    drain_queue_for_session(&state, "ws1").await.unwrap();

    // Active project + idle session ⇒ the drain consumes the message.
    assert!(state.db.get_queued_message("ws1").await.unwrap().is_none());
    // And persists the matching user event so the conversation log
    // reflects the actual delivery order.
    let events = state.db.events_tail("ws1", 10).await.unwrap();
    assert!(
        events.iter().any(|e| e.kind == "user"),
        "drain must append a user event"
    );
}

// ── cancel_worker_for_card_move ─────────────────────────────────────────

/// The card-move helper is fire-and-forget for the HTTP / MCP layers:
/// cancel the running provider AND drop any queued message so a later
/// drain on the cancel's completion can't resurrect a buffered turn
/// against a now-obsolete card step. Verifying the queued message drop
/// here is the essential half — provider cancel is exercised elsewhere
/// via the live mock provider's `is_running` flip.
#[tokio::test]
async fn cancel_worker_for_card_move_drops_queued_message() {
    let state = build_state().await;
    seed_project(&state).await;
    seed_card_with_worker(&state, "c1", "in_progress", "ws1").await;

    let ts = chrono::Utc::now().to_rfc3339();
    state
        .db
        .upsert_queued_message(NewQueuedMessage {
            session_id: "ws1".into(),
            text: "stale".into(),
            queued_at: ts,
            ..Default::default()
        })
        .await
        .unwrap();

    cancel_worker_for_card_move(&state, "ws1").await;

    assert!(
        state.db.get_queued_message("ws1").await.unwrap().is_none(),
        "card move must drop the queued message so drain can't resurrect"
    );
}

// ── bulk delete of project queued messages ──────────────────────────────

/// Project pause has to drop every queued message on the project so a
/// cancel's completion listener can't drain one into a fresh agent run
/// against the paused project. Without the bulk delete a single worker
/// with a queued reply would silently come back to life.
#[tokio::test]
async fn pause_clears_all_project_worker_queues() {
    let state = build_state().await;
    seed_project(&state).await;
    seed_card_with_worker(&state, "c1", "in_progress", "ws1").await;
    seed_card_with_worker(&state, "c2", "in_progress", "ws2").await;

    let ts = chrono::Utc::now().to_rfc3339();
    for sid in ["ws1", "ws2"] {
        state
            .db
            .upsert_queued_message(NewQueuedMessage {
                session_id: sid.into(),
                text: "queued".into(),
                queued_at: ts.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
    }

    let n = state
        .db
        .delete_queued_messages_for_project("p1")
        .await
        .unwrap();
    assert_eq!(n, 2, "every worker on the paused project should be cleared");
    assert!(state.db.get_queued_message("ws1").await.unwrap().is_none());
    assert!(state.db.get_queued_message("ws2").await.unwrap().is_none());
}

// ── worker-session resume ───────────────────────────────────────────────

/// Seed a card with no active worker but an intact resume link: `prev_ws`
/// worked `worker_step` on this card before being interrupted, and its
/// conversation survives (`conversation_id` set).
async fn seed_resumable_card(state: &Arc<AppState>, card_id: &str, step: &str, prev_ws: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    state
        .db
        .create_card(NewCard {
            id: card_id.into(),
            project_id: "p1".into(),
            title: format!("card-{card_id}"),
            description: "".into(),
            step: step.into(),
            priority: 1,
            workflow: "task".into(),
            model: Some("mock:echo".into()),
            effort: None,
            blocked: false,
            block_reason: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
        })
        .await
        .unwrap();
    state
        .db
        .create_session(NewSession {
            id: prev_ws.into(),
            name: format!("worker-{prev_ws}"),
            folder_id: "f1".into(),
            model: Some("mock:echo".into()),
            effort: None,
            is_worker: true,
            project_id: Some("p1".into()),
            card_id: Some(card_id.into()),
            conversation_id: Some("conv-1".into()),
            created_at: ts.clone(),
            last_activity: ts,
            worker_step: Some(step.into()),
            ..Default::default()
        })
        .await
        .unwrap();
    state
        .db
        .update_card(
            card_id,
            UpdateCard {
                last_worker_session_id: Some(Some(prev_ws.into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
}

/// A card still on the step its previous worker session was working (the
/// session ended without an intent, or the card detoured through backlog /
/// wont_do / blocked and came back) must get that SAME session back — the
/// provider resumes its conversation — not a freshly minted one.
#[tokio::test]
async fn orchestrator_resumes_previous_worker_session_on_same_step() {
    let state = build_state().await;
    seed_project(&state).await;
    seed_resumable_card(&state, "c1", "in_progress", "prev-ws").await;

    check_and_spawn_workers(&state).await;

    let card = state.db.get_card("c1").await.unwrap().unwrap();
    assert_eq!(
        card.worker_session_id.as_deref(),
        Some("prev-ws"),
        "orchestrator must resume the previous session, not mint a new one"
    );
    let sessions = state
        .db
        .list_worker_sessions_by_project("p1")
        .await
        .unwrap();
    assert_eq!(
        sessions.len(),
        1,
        "no duplicate worker session should be created on resume"
    );
}

/// Once the card has moved to a different real step, the old session's
/// resume link is severed and a later return to the original step gets a
/// fresh session.
#[tokio::test]
async fn orchestrator_spawns_fresh_session_after_card_advanced() {
    let state = build_state().await;
    seed_project(&state).await;
    seed_resumable_card(&state, "c1", "in_progress", "prev-ws").await;

    // User drags the card forward to `review`, then back to `in_progress`.
    for step in ["review", "in_progress"] {
        state
            .db
            .update_card(
                "c1",
                UpdateCard {
                    step: Some(step.into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
    let prev = state.db.get_session("prev-ws").await.unwrap().unwrap();
    assert_eq!(
        prev.worker_step, None,
        "moving to a different real step must sever the resume link"
    );

    check_and_spawn_workers(&state).await;

    let card = state.db.get_card("c1").await.unwrap().unwrap();
    let assigned = card.worker_session_id.as_deref().unwrap();
    assert_ne!(assigned, "prev-ws", "a severed session must not be resumed");
}

/// The resume link survives every detour the card can take without
/// actually advancing: back to backlog, into wont_do, and back again.
/// Only a move to a different real step (here `done`) severs it.
#[tokio::test]
async fn worker_resume_link_survives_backlog_and_wont_do_but_not_forward_moves() {
    let state = build_state().await;
    seed_project(&state).await;
    seed_resumable_card(&state, "c1", "in_progress", "prev-ws").await;

    let worker_step = |state: &Arc<AppState>| {
        let db = state.db.clone();
        async move {
            db.get_session("prev-ws")
                .await
                .unwrap()
                .unwrap()
                .worker_step
        }
    };

    for step in ["backlog", "in_progress", "wont_do", "in_progress"] {
        state
            .db
            .update_card(
                "c1",
                UpdateCard {
                    step: Some(step.into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            worker_step(&state).await.as_deref(),
            Some("in_progress"),
            "detour through {step} must keep the resume link"
        );
    }

    state
        .db
        .update_card(
            "c1",
            UpdateCard {
                step: Some("done".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        worker_step(&state).await,
        None,
        "moving to done must sever the resume link"
    );
}

/// A worker that exits without signalling any intent (crash, context
/// cutoff, plain stop) leaves the card on its step. The completion handler
/// must stamp `last_worker_session_id` so the respawn resumes this
/// session's conversation instead of starting a fresh agent.
#[tokio::test]
async fn worker_done_without_intent_keeps_resume_link() {
    let state = build_state().await;
    seed_project(&state).await;
    seed_card_with_worker(&state, "c1", "in_progress", "ws1").await;

    handle_worker_done(&state, "ws1").await;

    let card = state.db.get_card("c1").await.unwrap().unwrap();
    assert_eq!(card.worker_session_id, None, "slot must be freed");
    assert_eq!(
        card.last_worker_session_id.as_deref(),
        Some("ws1"),
        "resume link must point at the interrupted session"
    );
}
