//! `SpawnConfig.working_dir` must survive `SessionManager::send_message_locked`
//! into the provider dispatch instead of being silently overwritten by the
//! session's folder path. This is the worktree-per-card isolation fix: a
//! caller (the worker orchestrator) computes a card's git worktree via
//! `ensure_worktree` and puts it in `SpawnConfig.working_dir`; before the
//! fix, `send_message_locked` discarded it and always dispatched into the
//! shared folder.

use std::sync::Arc;

use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewAuthSession, NewFolder, NewSession, NewUser};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::message::UserMessage;
use peckboard::provider::mock::register_mock_provider;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::provider::stream::SpawnConfig;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::{AppState, TlsState};
use peckboard::ws::broadcaster::Broadcaster;

/// Build a minimal `AppState` with the mock provider registered, backed by
/// a real tempdir folder (`resolve_working_dir` canonicalizes + checks the
/// directory exists, so the folder path must be real, not `/tmp/f`).
async fn build_state(folder_path: &std::path::Path) -> (Arc<AppState>, String) {
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
        path: folder_path.to_str().unwrap().to_string(),
        created_at: ts.clone(),
    })
    .await
    .unwrap();
    db.create_session(NewSession {
        id: "s1".into(),
        name: "Worker".into(),
        folder_id: "f1".into(),
        model: Some("mock:echo".into()),
        created_at: ts.clone(),
        last_activity: ts,
        ..Default::default()
    })
    .await
    .unwrap();

    let state = Arc::new(AppState {
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
        tls: Arc::new(TlsState::new()),
    });

    std::mem::forget(tmp);
    (state, token)
}

async fn dispatch_and_read_working_dir(state: &Arc<AppState>, working_dir_config: &str) -> String {
    let mut completion_rx = state
        .session_manager
        .take_completion_rx()
        .await
        .expect("completion rx available");

    let lock = state.session_manager.lock_session("s1").await;
    state
        .session_manager
        .send_message_locked(
            &lock,
            UserMessage::from_text("hello"),
            &state.db,
            &state.broadcaster,
            SpawnConfig {
                working_dir: working_dir_config.to_string(),
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
        .find(|e| e.kind == "agent-start")
        .expect("agent-start event emitted");
    let data: serde_json::Value = serde_json::from_str(&started.data).unwrap();
    data["metadata"]["working_dir"]
        .as_str()
        .expect("working_dir present in Started metadata")
        .to_string()
}

/// A `SpawnConfig.working_dir` naming a real subdirectory of the session's
/// folder (the shape `ensure_worktree` produces for an isolated card) must
/// reach the provider unchanged, not get overwritten with the folder path.
#[tokio::test]
async fn worktree_working_dir_survives_dispatch() {
    let folder_tmp = tempfile::tempdir().unwrap();
    let worktree_dir = folder_tmp
        .path()
        .join(".peckboard")
        .join("worktrees")
        .join("abcd1234");
    std::fs::create_dir_all(&worktree_dir).unwrap();

    let (state, _token) = build_state(folder_tmp.path()).await;

    let seen = dispatch_and_read_working_dir(&state, worktree_dir.to_str().unwrap()).await;
    assert_eq!(
        std::fs::canonicalize(&seen).unwrap(),
        std::fs::canonicalize(&worktree_dir).unwrap(),
        "the worktree path must ride through to the provider dispatch"
    );

    std::mem::forget(folder_tmp);
}

/// A blank `working_dir` (the shape every non-worktree construction site
/// passes) must resolve to the session's folder path, same as before the
/// fix.
#[tokio::test]
async fn blank_working_dir_falls_back_to_folder() {
    let folder_tmp = tempfile::tempdir().unwrap();
    let (state, _token) = build_state(folder_tmp.path()).await;

    let seen = dispatch_and_read_working_dir(&state, "").await;
    assert_eq!(seen, folder_tmp.path().to_str().unwrap());

    std::mem::forget(folder_tmp);
}

/// A caller-supplied `working_dir` that escapes the session's folder must
/// be rejected and fall back to the folder path — a spawn must never be
/// pointed outside the project folder.
#[tokio::test]
async fn working_dir_outside_folder_is_rejected() {
    let folder_tmp = tempfile::tempdir().unwrap();
    let folder_dir = folder_tmp.path().join("folder");
    let outside_dir = folder_tmp.path().join("outside");
    std::fs::create_dir_all(&folder_dir).unwrap();
    std::fs::create_dir_all(&outside_dir).unwrap();

    let (state, _token) = build_state(&folder_dir).await;

    let seen = dispatch_and_read_working_dir(&state, outside_dir.to_str().unwrap()).await;
    assert_eq!(seen, folder_dir.to_str().unwrap());

    std::mem::forget(folder_tmp);
}
