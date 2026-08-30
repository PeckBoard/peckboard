//! `delete_session_core` must stop the agent and take the session's
//! durable queue with it.
//!
//! Deleting a session while its agent is mid-turn used to leave the child
//! process running (billing, and every `emit_event` append failing on the
//! events FK once the row was gone), and `queued_messages` has no FK to
//! `sessions` — so rows for a deleted session survived forever.
//!
//! This file locks both in: DELETE mid-turn cancels the run, and the
//! session's queued rows are gone afterwards.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewAuthSession, NewFolder, NewQueuedMessage, NewSession, NewUser};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::agent::{AgentProvider, SendMessageContext};
use peckboard::provider::manager::{MidTurnPolicy, SendOutcome, SessionManager};
use peckboard::provider::message::UserMessage;
use peckboard::provider::registry::{ProviderInfo, ProviderRegistry};
use peckboard::provider::stream::{ModelInfo, SpawnConfig};
use peckboard::routes::sessions::router;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::ws::broadcaster::Broadcaster;
use tower::ServiceExt;

/// A provider whose turn never ends on its own — it stays "running" until
/// someone cancels it, which is exactly the mid-turn state a delete has to
/// deal with.
struct HangingProvider {
    running: Arc<AtomicBool>,
    cancels: Arc<AtomicUsize>,
}

#[async_trait]
impl AgentProvider for HangingProvider {
    fn id(&self) -> &str {
        "hang"
    }

    async fn send_message(&self, _ctx: SendMessageContext) -> anyhow::Result<()> {
        self.running.store(true, Ordering::Release);
        Ok(())
    }

    async fn cancel(&self, _session_id: &str) {
        self.cancels.fetch_add(1, Ordering::AcqRel);
        self.running.store(false, Ordering::Release);
    }

    async fn interrupt(&self, session_id: &str) {
        self.cancel(session_id).await;
    }

    async fn write_stdin(&self, _session_id: &str, _text: &str) -> bool {
        false
    }

    async fn is_running(&self, _session_id: &str) -> bool {
        self.running.load(Ordering::Acquire)
    }

    async fn cleanup(&self) {}
    async fn shutdown(&self) {}
}

fn cfg() -> SpawnConfig {
    SpawnConfig {
        model: "hang:any".into(),
        effort: None,
        working_dir: String::new(),
        mcp_config_path: None,
        env: Default::default(),
        permission_mode: None,
        timeout_ms: None,
        metadata: serde_json::Value::Null,
        system_prompt_suffix: None,
        system_prompt_override: None,
        extra_allowed_tools: Vec::new(),
        extra_disallowed_tools: Vec::new(),
        is_worker: false,
        is_pre_hatcher: false,
    }
}

struct Env {
    state: Arc<AppState>,
    token: String,
    cancels: Arc<AtomicUsize>,
}

async fn build_env() -> Env {
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
    let cancels = Arc::new(AtomicUsize::new(0));
    provider_registry
        .register(
            Arc::new(HangingProvider {
                running: Arc::new(AtomicBool::new(false)),
                cancels: cancels.clone(),
            }),
            ProviderInfo {
                id: "hang".into(),
                display_name: "Hang".into(),
                models: vec![ModelInfo {
                    id: "any".into(),
                    display_name: "Any".into(),
                    capabilities: vec![],
                    tier: 0,
                }],
                effort_levels: vec![],
                capabilities: Default::default(),
            },
        )
        .await;
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
        path: "/tmp/f".into(),
        created_at: ts.clone(),
    })
    .await
    .unwrap();
    db.create_session(NewSession {
        id: "s-del".into(),
        name: "Chat".into(),
        folder_id: "f1".into(),
        model: Some("hang:any".into()),
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
    });

    std::mem::forget(tmp);

    Env {
        state,
        token,
        cancels,
    }
}

async fn delete_session(state: Arc<AppState>, token: &str, id: &str) -> StatusCode {
    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/api/sessions/{id}"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    router(state.clone())
        .with_state(state)
        .oneshot(req)
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn delete_mid_turn_cancels_agent_and_clears_queued_messages() {
    let Env {
        state,
        token,
        cancels,
    } = build_env().await;

    // Start a turn that never finishes on its own.
    let outcome = state
        .session_manager
        .send_or_queue(
            "s-del",
            UserMessage::from_text("go"),
            &state.db,
            &state.broadcaster,
            cfg(),
            MidTurnPolicy::Queue,
            false,
        )
        .await
        .unwrap();
    assert_eq!(outcome, SendOutcome::Started);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        state.session_manager.is_running("s-del").await,
        "turn must be in flight before the delete"
    );

    // Park a message in the durable FIFO — nothing else deletes these rows
    // for a session (no FK to `sessions`).
    state
        .db
        .enqueue_message(NewQueuedMessage {
            session_id: "s-del".into(),
            text: "later".into(),
            queued_at: chrono::Utc::now().to_rfc3339(),
            ..Default::default()
        })
        .await
        .unwrap();

    let status = delete_session(state.clone(), &token, "s-del").await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    assert!(
        !state.session_manager.is_running("s-del").await,
        "the agent must be cancelled by the delete, not left running until the idle reap"
    );
    assert!(
        cancels.load(Ordering::Acquire) >= 1,
        "delete must call cancel on the provider"
    );
    assert!(
        state
            .db
            .list_queued_messages("s-del")
            .await
            .unwrap()
            .is_empty(),
        "queued_messages rows for a deleted session must not survive"
    );
    assert!(state.db.get_session("s-del").await.unwrap().is_none());
}
