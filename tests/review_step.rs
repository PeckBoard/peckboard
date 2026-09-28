//! The shared `review` step: `finish_card` from a working step lands on
//! `review` (or straight on `done` when the project turned review off), and
//! the review step is always worked by a FRESH session on the project's
//! reviewer model/effort — never by resuming the implementer's session.

use std::sync::Arc;

use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::generate_jwt_secret;
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{
    NewCard, NewFolder, NewProject, NewSession, UpdateCard, UpdateProject,
};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::service::mcp_server::{McpTokenRegistry, McpToolRegistry, ToolCallContext};
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
    // Empty registry: dispatch fails, but the spawned session row (model,
    // effort, step) is still created — which is all these tests inspect.
    let provider_registry = Arc::new(ProviderRegistry::new());
    let session_manager = SessionManager::new(provider_registry.clone());
    let push_service = PushService::new(&config.data_dir);

    std::mem::forget(tmp);

    Arc::new(AppState {
        background: Default::default(),
        plugin_ws_tickets: Default::default(),
        device_registry: Default::default(),
        env_unlock: Arc::new(peckboard::service::env_vars::EnvUnlockRegistry::new()),
        config,
        db,
        plugins,
        builtin_plugins: Arc::new(BuiltinPluginRegistry::new()),
        jwt_secret: generate_jwt_secret(),
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
    })
}

/// Project p1 (task workflow, worker model `deadprovider:worker`) with card
/// c1 on `in_progress`, claimed by worker session w1 whose conversation is
/// resumable — so only the review-step rule keeps the orchestrator from
/// resuming it.
async fn seed(state: &AppState, review: UpdateProject) {
    let ts = chrono::Utc::now().to_rfc3339();
    let db = &state.db;
    db.create_folder(NewFolder {
        id: "f1".into(),
        name: "F".into(),
        path: "/tmp".into(),
        created_at: ts.clone(),
    })
    .await
    .unwrap();
    db.create_project(NewProject {
        id: "p1".into(),
        name: "P".into(),
        context: "".into(),
        folder_id: "f1".into(),
        worker_count: 1,
        status: "active".into(),
        workflow: "task".into(),
        model: Some("deadprovider:worker".into()),
        effort: Some("low".into()),
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
    db.update_project("p1", review).await.unwrap();
    db.create_card(NewCard {
        id: "c1".into(),
        project_id: "p1".into(),
        title: "Ship the thing".into(),
        description: "Acceptance: it ships.".into(),
        step: "in_progress".into(),
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
    db.create_session(NewSession {
        id: "w1".into(),
        name: "worker: Ship the thing".into(),
        folder_id: "f1".into(),
        model: Some("deadprovider:worker".into()),
        is_worker: true,
        project_id: Some("p1".into()),
        card_id: Some("c1".into()),
        conversation_id: Some("conv-w1".into()),
        created_at: ts.clone(),
        last_activity: ts.clone(),
        worker_step: Some("in_progress".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    db.update_card(
        "c1",
        UpdateCard {
            worker_session_id: Some(Some("w1".into())),
            ..Default::default()
        },
    )
    .await
    .unwrap();
}

async fn finish_as_worker(state: &AppState) -> serde_json::Value {
    let ctx = ToolCallContext {
        session_id: "w1".into(),
        project_id: Some("p1".into()),
        card_id: Some("c1".into()),
        folder_id: "f1".into(),
        db: Arc::new(state.db.clone()),
        broadcaster: state.broadcaster.clone(),
        provider_registry: None,
        data_dir: None,
        device_registry: None,
        background: None,
    };
    McpToolRegistry::new()
        .handle_tool_call(
            "finish_card",
            serde_json::json!({ "summary": "shipped it; branch feat/ship" }),
            &ctx,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn finish_goes_to_review_and_review_spawns_a_fresh_session_on_the_reviewer_model() {
    let state = build_state().await;
    seed(
        &state,
        UpdateProject {
            review_model: Some(Some("deadprovider:reviewer".into())),
            review_effort: Some(Some("high".into())),
            ..Default::default()
        },
    )
    .await;

    let res = finish_as_worker(&state).await;
    assert_eq!(res["to"], "review");
    let card = state.db.get_card("c1").await.unwrap().unwrap();
    assert_eq!(card.step, "review");
    assert_eq!(card.worker_session_id, None);
    // The implementer is recorded for the handoff, with its summary.
    assert_eq!(card.last_worker_session_id.as_deref(), Some("w1"));
    assert_eq!(
        card.handoff_context.as_deref(),
        Some("shipped it; branch feat/ship")
    );

    check_and_spawn_workers_at(&state, chrono::Utc::now()).await;

    let sessions = state.db.list_worker_sessions_by_card("c1").await.unwrap();
    let reviewer = sessions
        .iter()
        .find(|s| s.worker_step.as_deref() == Some("review"))
        .expect("the review step must spawn a reviewer session");
    assert_ne!(reviewer.id, "w1", "the implementer must never be resumed");
    assert_eq!(reviewer.model.as_deref(), Some("deadprovider:reviewer"));
    assert_eq!(reviewer.effort.as_deref(), Some("high"));
    assert!(reviewer.name.starts_with("review: "));
    // The implementer's session is kept (card history) and still findable
    // by card; the reviewer is a separate row.
    assert!(sessions.iter().any(|s| s.id == "w1"));
}

#[tokio::test]
async fn review_falls_back_to_the_project_model_and_effort() {
    let state = build_state().await;
    // Review on, no reviewer overrides.
    seed(
        &state,
        UpdateProject {
            review_enabled: Some(true),
            ..Default::default()
        },
    )
    .await;

    finish_as_worker(&state).await;
    check_and_spawn_workers_at(&state, chrono::Utc::now()).await;

    let sessions = state.db.list_worker_sessions_by_card("c1").await.unwrap();
    let reviewer = sessions
        .iter()
        .find(|s| s.worker_step.as_deref() == Some("review"))
        .expect("the review step must spawn a reviewer session");
    assert_ne!(reviewer.id, "w1");
    assert_eq!(reviewer.model.as_deref(), Some("deadprovider:worker"));
    assert_eq!(reviewer.effort.as_deref(), Some("low"));
}

#[tokio::test]
async fn review_disabled_finish_goes_straight_to_done() {
    let state = build_state().await;
    seed(
        &state,
        UpdateProject {
            review_enabled: Some(false),
            ..Default::default()
        },
    )
    .await;

    let res = finish_as_worker(&state).await;
    assert_eq!(res["to"], "done");
    let card = state.db.get_card("c1").await.unwrap().unwrap();
    assert_eq!(card.step, "done");
}
