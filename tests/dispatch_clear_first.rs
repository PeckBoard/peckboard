//! `LiveHost::dispatch_capture` with `clear_first` must wipe the session and
//! THEN dispatch, ordered inside one task. The flag exists because a plugin
//! issuing separate `peckboard_clear_session` + `peckboard_dispatch_capture`
//! host calls gets two unordered `rt.spawn`s — the wipe can land after the
//! dispatch and kill the run it just started (the project-planner's
//! per-question context reset would then hang the interview on "thinking").
//!
//! Ordering is observable from the final event log: the pre-dispatch marker
//! event must be gone (wipe ran) while the dispatched run's events survive
//! (wipe ran FIRST). A wipe that ran second would leave the log empty; no
//! wipe would leave the marker in place.

use std::sync::Arc;

use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::generate_jwt_secret;
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewSession};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::host::LiveHost;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::mock::register_mock_provider;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::service::mcp_server::{AppLiveHost, McpTokenRegistry};
use peckboard::service::push::PushService;
use peckboard::state::{AppState, TlsState};
use peckboard::ws::broadcaster::Broadcaster;

/// Minimal `AppState` with the mock provider registered and one mock-backed
/// session (`s1`) in a real tempdir folder (dispatch resolves and checks the
/// working dir, so the folder path must exist).
async fn build_state(folder_path: &std::path::Path) -> Arc<AppState> {
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
    let provider_registry = Arc::new(ProviderRegistry::new());
    register_mock_provider(&provider_registry).await;
    let session_manager = SessionManager::new(provider_registry.clone());
    let push_service = PushService::new(&config.data_dir);

    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: "f1".into(),
        name: "F".into(),
        path: folder_path.to_str().unwrap().to_string(),
        created_at: ts.clone(),
    })
    .await
    .unwrap();
    db.create_session(NewSession {
        id: "s1".into(),
        name: "Planner interview".into(),
        folder_id: "f1".into(),
        model: Some("mock:echo".into()),
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
        tls: Arc::new(TlsState::new()),
    });

    std::mem::forget(tmp);
    state
}

/// Dispatch through the real `AppLiveHost` and wait for the mock turn to
/// finish, then return the session's full event log.
async fn dispatch_and_settle(
    state: &Arc<AppState>,
    clear_first: bool,
) -> Vec<peckboard::db::models::Event> {
    let mut completion_rx = state
        .session_manager
        .take_completion_rx()
        .await
        .expect("completion rx available");

    let live = AppLiveHost::new(state, tokio::runtime::Handle::current());
    live.dispatch_capture("s1".into(), "the user answered: hats".into(), clear_first);

    tokio::time::timeout(std::time::Duration::from_secs(10), completion_rx.recv())
        .await
        .expect("turn completes")
        .expect("channel open");

    state.db.events_tail("s1", 100).await.unwrap()
}

#[tokio::test]
async fn clear_first_wipes_then_dispatches_in_order() {
    let folder_tmp = tempfile::tempdir().unwrap();
    let state = build_state(folder_tmp.path()).await;

    state
        .db
        .append_event("s1", "user", serde_json::json!({ "text": "stale history" }))
        .await
        .unwrap();

    let events = dispatch_and_settle(&state, true).await;

    assert!(
        !events.iter().any(|e| e.data.contains("stale history")),
        "clear_first must wipe the pre-dispatch transcript",
    );
    assert!(
        !events.is_empty() && events.iter().any(|e| e.kind == "agent-start"),
        "the dispatched run must survive the wipe (wipe ran second?): {events:?}",
    );
    // The wipe restarts per-session seq at 1 — the planner resets its stored
    // high-water mark to 0 on this contract.
    assert_eq!(
        events.first().unwrap().seq,
        1,
        "seq must restart after the wipe"
    );

    std::mem::forget(folder_tmp);
}

#[tokio::test]
async fn plain_dispatch_keeps_the_transcript() {
    let folder_tmp = tempfile::tempdir().unwrap();
    let state = build_state(folder_tmp.path()).await;

    state
        .db
        .append_event("s1", "user", serde_json::json!({ "text": "stale history" }))
        .await
        .unwrap();

    let events = dispatch_and_settle(&state, false).await;

    assert!(
        events.iter().any(|e| e.data.contains("stale history")),
        "without clear_first the transcript must survive",
    );
    assert!(events.iter().any(|e| e.kind == "agent-start"));

    std::mem::forget(folder_tmp);
}
