//! End-to-end regression test for the reported bug: "the Ollama provider is
//! not terminating sessions currently in progress — the model keeps streaming
//! text into the chat after I hit Interrupt/Terminate."
//!
//! The Ollama provider's own cancel path is correct (see
//! `tests/ollama_provider.rs`). The failure is at the integration level:
//! Ollama is a per-turn provider, so a follow-up sent while a run is
//! streaming gets QUEUED, and the completion listener drains that queue on
//! every completion — including the synthetic one produced by
//! interrupt/terminate. So an explicit stop immediately respawned a fresh run
//! and the chat kept streaming.
//!
//! These tests drive the real HTTP routes (`/interrupt`, `/terminate`)
//! against a stub Ollama firehose, with a replica of the `main.rs` completion
//! listener running, and assert that after an explicit stop nothing respawns.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewAuthSession, NewFolder, NewSession, NewUser};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::plugin::settings::{FieldKind, PluginSettingsStore, SettingField, SettingsSchema};
use peckboard::provider::manager::SessionManager;
use peckboard::provider::ollama::{OllamaProvider, default_models};
use peckboard::provider::registry::{ProviderInfo, ProviderRegistry};
use peckboard::provider::stream::SpawnConfig;
use peckboard::routes::sessions::router;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::ws::broadcaster::Broadcaster;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tower::ServiceExt;

/// Stub Ollama that streams NDJSON text chunks forever (until the client
/// disconnects). Accepts repeated connections so a respawned run gets served
/// too — important: if the bug respawns a run, this stub keeps feeding it.
async fn spawn_firehose() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _peer)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = tokio::time::timeout(Duration::from_secs(5), sock.read(&mut buf)).await;
                let headers = "HTTP/1.1 200 OK\r\n\
                               Content-Type: application/x-ndjson\r\n\
                               Transfer-Encoding: chunked\r\n\r\n";
                if sock.write_all(headers.as_bytes()).await.is_err() {
                    return;
                }
                let line = "{\"message\":{\"content\":\"tok \"},\"done\":false}\n";
                let chunk = format!("{:x}\r\n{}\r\n", line.len(), line);
                loop {
                    if sock.write_all(chunk.as_bytes()).await.is_err() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(3)).await;
                }
            });
        }
    });
    format!("http://{}", addr)
}

fn ollama_schema() -> SettingsSchema {
    SettingsSchema::new(vec![SettingField {
        key: "base_url".into(),
        title: "Base URL".into(),
        description: None,
        required: true,
        kind: FieldKind::Url {
            default: Some("http://localhost:11434".into()),
            placeholder: None,
        },
    }])
}

fn config() -> SpawnConfig {
    SpawnConfig {
        model: "ollama:llama3.1".into(),
        ..Default::default()
    }
}

async fn count_text(db: &Db) -> usize {
    db.events_tail("s1", 8192)
        .await
        .unwrap()
        .iter()
        .filter(|e| e.kind == "agent-text")
        .count()
}

/// Build a real `AppState` with the Ollama provider registered (pointed at
/// `base_url`), an auth token for the routes, and the completion-listener
/// loop from `main.rs` running. Returns `(state, token)`.
async fn build_state(base_url: String) -> (Arc<AppState>, String) {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config {
        port: 0,
        https_port: 0,
        host: "127.0.0.1".into(),
        data_dir: tmp.path().to_path_buf(),
        mdns: false,
        keep_alive_hours: 0,
        provider_send_timeout_secs: 300,
    };
    let db = Db::in_memory().unwrap();
    let plugins = Arc::new(PluginManager::new(&cfg.data_dir, db.clone()));
    let provider_registry = Arc::new(ProviderRegistry::new());

    db.set_plugin_setting("ollama", "base_url", &serde_json::Value::String(base_url))
        .await
        .unwrap();
    let store = PluginSettingsStore::new("ollama", ollama_schema(), db.clone());
    provider_registry
        .register(
            Arc::new(OllamaProvider::new(store)),
            ProviderInfo {
                id: "ollama".into(),
                display_name: "Ollama".into(),
                models: default_models(),
                effort_levels: vec![],
            },
        )
        .await;

    let session_manager = SessionManager::new(provider_registry.clone());
    let completion_rx = session_manager.take_completion_rx().await.unwrap();
    let jwt_secret = generate_jwt_secret();

    // Auth: one admin user + a live session token for the routes.
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
        id: "s1".into(),
        name: "Chat".into(),
        folder_id: "f1".into(),
        model: Some("ollama:llama3.1".into()),
        created_at: ts.clone(),
        last_activity: ts,
        ..Default::default()
    })
    .await
    .unwrap();

    let state = Arc::new(AppState {
        config: cfg,
        db: db.clone(),
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
        push_service: PushService::new(tmp.path()),
    });
    std::mem::forget(tmp);

    // Replica of the main.rs completion listener (the interactive slice):
    // drain any queued message on every completion.
    {
        let listener_state = state.clone();
        let mut rx = completion_rx;
        tokio::spawn(async move {
            while let Some(completion) = rx.recv().await {
                let sid = completion.session_id.clone();
                let _ =
                    peckboard::worker::orchestrator::drain_queue_for_session(&listener_state, &sid)
                        .await;
            }
        });
    }

    (state, token)
}

/// Drive: send → queue a follow-up → start streaming → assert the queue is
/// armed. Shared setup for both the interrupt and terminate cases.
async fn send_and_queue_followup(state: &Arc<AppState>) {
    state
        .session_manager
        .send_or_queue(
            "s1",
            "first".into(),
            &state.db,
            &state.broadcaster,
            config(),
        )
        .await
        .unwrap();

    for _ in 0..200 {
        if count_text(&state.db).await > 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(count_text(&state.db).await > 3, "run A should be streaming");

    state
        .session_manager
        .send_or_queue(
            "s1",
            "second".into(),
            &state.db,
            &state.broadcaster,
            config(),
        )
        .await
        .unwrap();
    assert!(
        state.db.get_queued_message("s1").await.unwrap().is_some(),
        "the follow-up must have been queued (Ollama is per-turn)"
    );
}

async fn post(state: &Arc<AppState>, token: &str, path: &str) -> StatusCode {
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    router(state.clone())
        .with_state(state.clone())
        .oneshot(req)
        .await
        .unwrap()
        .status()
}

/// After hitting `path`, no further text may stream and the session must not
/// be running — i.e. the explicit stop actually stopped, with no respawn from
/// the queue.
async fn assert_stop_is_final(state: &Arc<AppState>, path_status: StatusCode, label: &str) {
    assert_eq!(
        path_status,
        StatusCode::NO_CONTENT,
        "{label} route should return 204"
    );

    let after = count_text(&state.db).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let later = count_text(&state.db).await;

    assert!(
        !state.session_manager.is_running("s1").await,
        "{label}: session must NOT be running afterwards (it respawned from the queue)"
    );
    assert_eq!(
        later,
        after,
        "{label}: no new text may stream afterwards, but {} more agent-text events arrived \
         — the queued follow-up respawned the run",
        later.saturating_sub(after)
    );
    assert!(
        state.db.get_queued_message("s1").await.unwrap().is_none(),
        "{label}: the queued follow-up must be cleared by an explicit stop"
    );
}

/// Terminate is a hard "fresh start on the next message" stop: it must
/// discard the queued follow-up so the completion listener doesn't respawn a
/// run. This is the bug the user reported ("model keeps streaming after I hit
/// Terminate").
#[tokio::test]
async fn terminate_clears_queue_and_does_not_respawn_ollama() {
    let base_url = spawn_firehose().await;
    let (state, token) = build_state(base_url).await;
    send_and_queue_followup(&state).await;

    let status = post(&state, &token, "/api/sessions/s1/terminate").await;
    assert_stop_is_final(&state, status, "terminate").await;
}

/// Interrupt is, by design, "release the current turn so my queued follow-up
/// runs" — NOT a hard stop. So after interrupt the queue must DRAIN (be
/// consumed into a fresh run), not be discarded. This locks that distinction
/// against the terminate behavior above (and mirrors the session-lifecycle
/// e2e + `drain_queued_delivers_after_interrupted_run`).
#[tokio::test]
async fn interrupt_drains_queued_followup_into_fresh_run() {
    let base_url = spawn_firehose().await;
    let (state, token) = build_state(base_url).await;
    send_and_queue_followup(&state).await;

    let status = post(&state, &token, "/api/sessions/s1/interrupt").await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "interrupt route should return 204"
    );

    // The queued follow-up should be drained (consumed) into a fresh run:
    // the queue empties AND a run is in flight again.
    let mut drained_into_run = false;
    for _ in 0..300 {
        let queue_empty = state.db.get_queued_message("s1").await.unwrap().is_none();
        let running = state.session_manager.is_running("s1").await;
        if queue_empty && running {
            drained_into_run = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        drained_into_run,
        "interrupt must drain the queued follow-up into a fresh run (release-and-continue)"
    );
}
