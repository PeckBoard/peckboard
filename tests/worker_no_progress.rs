//! Regression test for the runaway respawn loop: a worker whose turn ends
//! without calling `complete_step` / `finish_card` / `wont_do_card` must
//! NOT be respawned forever. `mock:happy-path` reproduces this
//! deterministically -- it emits Text -> ToolStart/ToolEnd -> Text ->
//! Completed and never touches a card-lifecycle tool -- so this test
//! exercises the real orchestrator + completion path end to end.
//!
//! See `pipeline::{NO_PROGRESS_KIND, BLOCK_AFTER_NO_PROGRESS,
//! count_consecutive_no_progress, no_progress_backoff_secs}` and the
//! backoff gate in `check_and_spawn_workers_at`.

use std::sync::Arc;
use std::time::Duration;

use peckboard::auth::rate_limit::RateLimiter;
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewCard, NewFolder, NewProject};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::mock::register_mock_provider;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::worker::orchestrator::{check_and_spawn_workers_at, handle_worker_done};
use peckboard::worker::pipeline::BLOCK_AFTER_NO_PROGRESS;
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
        ssh_vault_key: vec![0u8; 32],
        mfa_vault_key: vec![0u8; 32],
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

/// Seed a folder + an active, single-worker project pinned to
/// `mock:happy-path` + one backlog card.
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
            title: "does nothing".into(),
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

/// The runaway-loop repro: a worker that completes its turn without
/// calling complete_step/finish_card/wont_do_card must stop being
/// respawned within `BLOCK_AFTER_NO_PROGRESS` attempts, and the card must
/// end up blocked instead of spinning forever.
#[tokio::test]
async fn no_progress_worker_is_bounded_and_card_gets_blocked() {
    let state = build_state().await;
    seed_project_and_card(&state).await;

    let mut completion_rx = state
        .session_manager
        .take_completion_rx()
        .await
        .expect("completion rx available");

    let mut now = chrono::Utc::now();
    let mut spawn_count = 0u32;

    for _ in 0..(BLOCK_AFTER_NO_PROGRESS + 3) {
        check_and_spawn_workers_at(&state, now).await;

        let card = state.db.get_card("c1").await.unwrap().unwrap();
        if card.blocked {
            break;
        }
        let Some(session_id) = card.worker_session_id.clone() else {
            // Backoff is holding this tick (shouldn't happen given we jump
            // `now` well past the max backoff each iteration, but don't
            // spin tightly if it does).
            now += chrono::Duration::seconds(310);
            continue;
        };
        spawn_count += 1;

        let completion = tokio::time::timeout(Duration::from_secs(5), completion_rx.recv())
            .await
            .expect("mock happy-path turn must complete")
            .expect("completion channel open");
        assert_eq!(completion.session_id, session_id);
        assert!(
            completion.completed,
            "happy-path must exit cleanly, not crash"
        );

        handle_worker_done(&state, &session_id).await;

        // Jump the virtual clock well past the max no-progress backoff
        // (300s) so the next tick isn't gated by it -- this test bounds
        // the attempt COUNT, not the delay curve (covered by the
        // `pipeline` unit tests).
        now += chrono::Duration::seconds(310);
    }

    let card = state.db.get_card("c1").await.unwrap().unwrap();
    assert!(
        card.blocked,
        "card must be blocked after repeated no-progress completions, got step={}",
        card.step
    );
    assert!(
        card.block_reason.as_deref().is_some_and(|r| !r.is_empty()),
        "block_reason must explain why the card was blocked"
    );
    assert!(
        spawn_count <= BLOCK_AFTER_NO_PROGRESS,
        "spawn count must be bounded by BLOCK_AFTER_NO_PROGRESS ({BLOCK_AFTER_NO_PROGRESS}), got {spawn_count}"
    );

    // A further tick, even with the clock far in the future, must not
    // spawn anything -- the card is blocked outright, not merely backed off.
    now += chrono::Duration::seconds(3600);
    check_and_spawn_workers_at(&state, now).await;
    let card = state.db.get_card("c1").await.unwrap().unwrap();
    assert!(
        card.worker_session_id.is_none(),
        "a blocked card must not be respawned"
    );
}
