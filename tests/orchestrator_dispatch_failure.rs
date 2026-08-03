//! Regression for a card pinned to a dead model (deleted account,
//! uninstalled provider, stale project model): `spawn_worker_for_card`
//! used to mint a brand-new session row on every 5s tick forever, since
//! a dispatch failure released the claim but left a session with neither
//! `conversation_id` nor `pending_handover_doc` -- which fails the resume
//! filter -- and never counted toward the crash-based auto-pause.
//!
//! This locks in the fix: dispatch failures reuse the dead session row
//! (bounded row count) and count as crashes (auto-pause after two in a
//! row, surfacing the failure to the user instead of stalling silently).

use std::sync::Arc;

use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::generate_jwt_secret;
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewCard, NewFolder, NewProject};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::worker::orchestrator::check_and_spawn_workers_at;
use peckboard::ws::broadcaster::Broadcaster;

async fn build_state() -> Arc<AppState> {
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
    // Empty registry: no provider is ever registered, so `model: "default"`
    // (or any card model) fails to resolve at dispatch time -- exactly what
    // a deleted Claude account / uninstalled provider looks like.
    let provider_registry = Arc::new(ProviderRegistry::new());
    let session_manager = SessionManager::new(provider_registry.clone());
    let push_service = PushService::new(&config.data_dir);

    std::mem::forget(tmp);

    Arc::new(AppState {
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
        tls: Arc::new(peckboard::state::TlsState::new()),
    })
}

/// An active project with one worker slot and a backlog card pinned to a
/// model no registered provider can serve.
async fn seed_dead_model_card(state: &AppState) {
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
            model: Some("deadprovider:ghost-model".into()),
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
            title: "Ship the thing".into(),
            description: "".into(),
            step: "backlog".into(),
            priority: 1,
            workflow: "task".into(),
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
}

#[tokio::test]
async fn dead_model_card_stays_bounded_and_pauses_the_project() {
    let state = build_state().await;
    seed_dead_model_card(&state).await;

    // Simulate several 5s orchestrator ticks. Each one, prior to the fix,
    // minted a brand-new worker session row for the same card.
    for _ in 0..5 {
        check_and_spawn_workers_at(&state, chrono::Utc::now()).await;
    }

    let sessions = state
        .db
        .list_worker_sessions_by_card("c1")
        .await
        .unwrap_or_default();
    assert!(
        sessions.len() <= 1,
        "dispatch failures must reuse the dead session row instead of \
         minting a new one every tick, got {} rows",
        sessions.len()
    );

    let project = state.db.get_project("p1").await.unwrap().unwrap();
    assert_eq!(
        project.status, "paused",
        "repeated dispatch failures must auto-pause the project instead \
         of stalling silently forever"
    );
    assert!(
        project.pause_reason.is_some(),
        "the pause must carry a reason the user can see on the project banner"
    );

    let card = state.db.get_card("c1").await.unwrap().unwrap();
    assert!(
        card.worker_session_id.is_none(),
        "a card that never got a live agent run must not stay claimed"
    );
}
