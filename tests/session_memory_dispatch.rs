//! The session memory pool must reach the provider on every dispatch as a
//! "Session memory" section of the per-spawn system-prompt suffix — that is
//! what makes the pool visible to the agent after a clear, a resume, or a
//! compaction. An empty pool must add nothing (no prompt tokens spent).

use std::sync::Arc;

use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::generate_jwt_secret;
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewSession};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::agent::ProcessCompletion;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::message::UserMessage;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::provider::stream::SpawnConfig;
use peckboard::provider::test_double::register_mock_provider;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::{AppState, TlsState};
use peckboard::ws::broadcaster::Broadcaster;

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
        name: "Chat".into(),
        folder_id: "f1".into(),
        model: Some("mock:echo".into()),
        created_at: ts.clone(),
        last_activity: ts,
        ..Default::default()
    })
    .await
    .unwrap();

    let state = Arc::new(AppState {
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
        remote_access: peckboard::service::remote_access::RemoteAccess::inert(),
        tls: Arc::new(TlsState::new()),
    });
    std::mem::forget(tmp);
    state
}

/// Dispatch one turn and return the `system_prompt_suffix` the test double
/// recorded in its `Started` metadata (`null` when none was passed).
async fn dispatch_and_read_suffix(
    state: &Arc<AppState>,
    completion_rx: &mut tokio::sync::mpsc::Receiver<ProcessCompletion>,
    suffix: Option<&str>,
) -> serde_json::Value {
    let lock = state.session_manager.lock_session("s1").await;
    state
        .session_manager
        .send_message_locked(
            &lock,
            UserMessage::from_text("hello"),
            &state.db,
            &state.broadcaster,
            SpawnConfig {
                system_prompt_suffix: suffix.map(str::to_string),
                ..Default::default()
            },
        )
        .await
        .expect("mock dispatch succeeds");
    drop(lock);
    tokio::time::timeout(std::time::Duration::from_secs(5), completion_rx.recv())
        .await
        .expect("turn completes")
        .expect("channel open");

    let events = state.db.events_tail("s1", 50).await.unwrap();
    let started = events
        .iter()
        .rev()
        .find(|e| e.kind == "agent-start")
        .expect("agent-start event emitted");
    let data: serde_json::Value = serde_json::from_str(&started.data).unwrap();
    data["metadata"]["system_prompt_suffix"].clone()
}

#[tokio::test]
async fn memory_pool_is_appended_to_the_spawn_prompt_suffix() {
    let folder = tempfile::tempdir().unwrap();
    let state = build_state(folder.path()).await;
    // Taken once: the manager hands out its completion channel a single time.
    let mut completion_rx = state
        .session_manager
        .take_completion_rx()
        .await
        .expect("completion rx available");

    // Empty pool: nothing is added, the caller's suffix passes through.
    assert_eq!(
        dispatch_and_read_suffix(&state, &mut completion_rx, None).await,
        serde_json::Value::Null,
        "an empty pool must not add a section"
    );

    let a = state
        .db
        .add_session_memory("s1", "user prefers tabs")
        .await
        .unwrap();
    let b = state
        .db
        .add_session_memory("s1", "deploy target is prod-eu")
        .await
        .unwrap();

    // Memories survive what clear_session_core does to the transcript, so
    // the very next dispatch still carries them.
    state.db.delete_events_by_session("s1").await.unwrap();

    let suffix =
        dispatch_and_read_suffix(&state, &mut completion_rx, Some("# Repeating Task Context"))
            .await;
    let text = suffix.as_str().expect("suffix present");
    assert!(
        text.starts_with("# Repeating Task Context\n"),
        "caller's suffix stays first: {text}"
    );
    assert!(text.contains("# Session memory"), "got: {text}");
    assert!(
        text.contains(&format!("- [{}] user prefers tabs", a.id)),
        "got: {text}"
    );
    assert!(
        text.contains(&format!("- [{}] deploy target is prod-eu", b.id)),
        "got: {text}"
    );

    std::mem::forget(folder);
}
