//! Regression for a leaked concurrency slot: when the async first-turn
//! dispatch after `spawn_subagent` fails (bad model, dead provider, ...),
//! `fail_subagent_dispatch` must stamp `subagent_completed_at` (freeing the
//! parent's slot) and report a CRASHED result back to the parent — the same
//! path a real mid-run crash uses. Before the fix, this dispatch failure
//! only logged a warning and the child row stayed `subagent_completed_at
//! IS NULL` forever, so `count_active_subagents` counted it until server
//! restart.

use std::sync::Arc;

use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::generate_jwt_secret;
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewSession};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
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

async fn seed_folder(db: &Db, id: &str) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: id.into(),
        name: id.into(),
        path: format!("/tmp/subagent-dispatch-failure-test/{id}"),
        created_at: ts,
    })
    .await
    .unwrap();
}

async fn seed_session(db: &Db, id: &str, folder_id: &str, parent: Option<&str>) {
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_session(NewSession {
        id: id.into(),
        name: id.into(),
        folder_id: folder_id.into(),
        model: Some("claude:claude-opus-4-8".into()),
        created_at: ts.clone(),
        last_activity: ts,
        parent_session_id: parent.map(|s| s.to_string()),
        expert_kind: parent.map(|_| "subagent".to_string()),
        is_expert: parent.is_some(),
        ..Default::default()
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn dispatch_failure_frees_slot_and_reports_crash_to_parent() {
    let state = build_state().await;
    seed_folder(&state.db, "f1").await;
    seed_session(&state.db, "parent", "f1", None).await;
    seed_session(&state.db, "sub: scout", "f1", Some("parent")).await;

    assert_eq!(state.db.count_active_subagents("parent").await.unwrap(), 1);

    peckboard::subagent::fail_subagent_dispatch(&state, "sub: scout", "boom: dispatch failed")
        .await;

    // Slot freed: the child no longer counts as active.
    assert_eq!(state.db.count_active_subagents("parent").await.unwrap(), 0);
    let child = state.db.get_session("sub: scout").await.unwrap().unwrap();
    assert!(child.subagent_completed_at.is_some());

    // Parent got a CRASHED report on the same channel as a normal result.
    let events = state.db.events_tail("parent", 10).await.unwrap();
    let reported = events.iter().find(|e| e.kind == "user").unwrap();
    let data: serde_json::Value = serde_json::from_str(&reported.data).unwrap();
    assert_eq!(data["source"], "subagent-result");
    assert!(data["text"].as_str().unwrap().contains("CRASHED"));
    assert!(
        data["text"]
            .as_str()
            .unwrap()
            .contains("boom: dispatch failed")
    );

    // Idempotent: a second failure report for the same child is a no-op
    // (claim already won), so the parent doesn't see a duplicate event.
    peckboard::subagent::fail_subagent_dispatch(&state, "sub: scout", "boom again").await;
    let events_after = state.db.events_tail("parent", 10).await.unwrap();
    assert_eq!(events.len(), events_after.len());
}

#[tokio::test]
async fn dispatch_failure_for_unknown_child_is_a_noop() {
    let state = build_state().await;
    // Should not panic; just logs and returns.
    peckboard::subagent::fail_subagent_dispatch(&state, "does-not-exist", "boom").await;
}
