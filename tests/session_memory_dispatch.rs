//! Memory must reach the provider on every dispatch as the "Memory" section
//! of the per-spawn system-prompt suffix: the rules for long- and short-term
//! notes (always), the folder's long-term notes and the session's own notes.
//! That is what makes memory visible to the agent after a clear, a resume,
//! or a compaction.

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
        terminals: peckboard::terminal::TerminalManager::inert(),
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

    // Nothing remembered yet: the rules still reach the agent (that is how
    // every provider learns what to save where), but no empty note lists.
    let rules = dispatch_and_read_suffix(&state, &mut completion_rx, None).await;
    let rules = rules.as_str().expect("memory rules always present");
    assert!(rules.contains("# Memory"), "got: {rules}");
    assert!(rules.contains("**Long-term**") && rules.contains("**Short-term**"));
    assert!(!rules.contains("## "), "no empty sections: {rules}");

    let kept = state
        .db
        .add_session_memory("s1", "user prefers tabs")
        .await
        .unwrap();
    let short = state
        .db
        .add_short_term_memory("s1", "deploying to prod-eu", 24)
        .await
        .unwrap();
    let long = state
        .db
        .add_folder_memory("f1", "architecture", "One binary", "axum + sqlite", None)
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
    assert!(
        text.contains(&format!("- [{}] **One binary** — axum + sqlite", long.id)),
        "the folder's long-term note: {text}"
    );
    assert!(
        text.contains(&format!(
            "- [{}] deploying to prod-eu (expires in 24h)",
            short.id
        )),
        "the session's short-term note: {text}"
    );
    assert!(
        text.contains(&format!("- [{}] user prefers tabs", kept.id)),
        "a note without expiry: {text}"
    );

    std::mem::forget(folder);
}
