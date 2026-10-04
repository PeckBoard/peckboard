//! Assistant mirror end to end: settings API (validation, secrets never
//! returned), a simulated voice turn reaching a local Slack/Discord
//! receiver, and the email digest reaching a local SMTP sink only while
//! the Assistant panel is not watched.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
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
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::service::assistant_mirror::AssistantMirror;
use peckboard::service::assistant_mirror::format::INTERRUPT_MARKER;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::ws::broadcaster::{Broadcaster, WsEvent};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tower::ServiceExt;

type Captured = Arc<Mutex<Vec<(String, Value)>>>;

async fn seed_user(db: &Db, secret: &[u8], id: &str, role: &str, auth_id: &str) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    db.create_user(NewUser {
        id: id.into(),
        username: format!("user-{id}"),
        email: None,
        password_hash: "h".into(),
        role: role.into(),
        created_at: now.clone(),
        updated_at: now,
    })
    .await
    .unwrap();
    let (token, _) = create_token(secret, id, role, auth_id).unwrap();
    db.create_auth_session(NewAuthSession {
        id: auth_id.into(),
        user_id: id.into(),
        token_hash: hash_token(&token),
        created_at: 1_000_000,
        expires_at: 1_000_000 + 7 * 24 * 60 * 60,
        user_agent: None,
        ip_address: None,
    })
    .await
    .unwrap();
    token
}

async fn build_state() -> (Arc<AppState>, String, String) {
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
    let jwt_secret = generate_jwt_secret();
    let admin = seed_user(&db, &jwt_secret, "u1", "admin", "as1").await;
    let user = seed_user(&db, &jwt_secret, "u2", "user", "as2").await;
    let provider_registry = Arc::new(ProviderRegistry::new());
    let state = Arc::new(AppState {
        background: Default::default(),
        plugin_ws_tickets: Default::default(),
        device_registry: Default::default(),
        env_unlock: Arc::new(peckboard::service::env_vars::EnvUnlockRegistry::new()),
        plugins: Arc::new(PluginManager::new(&config.data_dir, db.clone())),
        builtin_plugins: Arc::new(BuiltinPluginRegistry::new()),
        password_change_limiter: RateLimiter::<String>::new(5),
        login_limiter: RateLimiter::new(60),
        ssh_vault_key: peckboard::service::ssh_keys::load_or_create_vault_key(&config.data_dir)
            .unwrap(),
        mfa_vault_key: peckboard::auth::mfa::vault::load_or_create_vault_key(&config.data_dir)
            .unwrap(),
        broadcaster: Broadcaster::new(),
        session_manager: SessionManager::new(provider_registry.clone()),
        provider_registry,
        repeating_task_manager: peckboard::repeating::RepeatingTaskManager::new(),
        run_auditor: peckboard::repeating::RunAuditor::new(),
        mcp_tokens: McpTokenRegistry::new(),
        push_service: PushService::new(&config.data_dir),
        remote_access: peckboard::service::remote_access::RemoteAccess::inert(),
        tls: Arc::new(peckboard::state::TlsState::new()),
        jwt_secret,
        config,
        db,
    });
    std::mem::forget(tmp);
    (state, admin, user)
}

/// Local webhook receiver: records `(path, json body)` of every POST.
async fn spawn_receiver() -> (SocketAddr, Captured) {
    let got: Captured = Default::default();
    let sink = got.clone();
    let app = axum::Router::new().route(
        "/{*path}",
        axum::routing::post(
            move |uri: axum::http::Uri, axum::Json(v): axum::Json<Value>| {
                let sink = sink.clone();
                async move {
                    sink.lock().unwrap().push((uri.path().to_string(), v));
                    "ok"
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, got)
}

/// Minimal SMTP sink (no TLS, no auth): records every DATA payload.
async fn spawn_smtp() -> (u16, Arc<Mutex<Vec<String>>>) {
    let mails: Arc<Mutex<Vec<String>>> = Default::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let sink = mails.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let sink = sink.clone();
            tokio::spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut r = BufReader::new(r);
                let _ = w.write_all(b"220 sink ESMTP\r\n").await;
                let mut line = String::new();
                loop {
                    line.clear();
                    if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let cmd = line.trim_end().to_ascii_uppercase();
                    let reply: &[u8] = if cmd.starts_with("EHLO") || cmd.starts_with("HELO") {
                        b"250-sink\r\n250 8BITMIME\r\n"
                    } else if cmd == "DATA" {
                        let _ = w.write_all(b"354 go\r\n").await;
                        let mut data = String::new();
                        loop {
                            line.clear();
                            if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                                return;
                            }
                            if line == ".\r\n" {
                                break;
                            }
                            data.push_str(&line);
                        }
                        sink.lock().unwrap().push(data);
                        b"250 queued\r\n"
                    } else if cmd == "QUIT" {
                        let _ = w.write_all(b"221 bye\r\n").await;
                        return;
                    } else {
                        b"250 ok\r\n"
                    };
                    let _ = w.write_all(reply).await;
                }
            });
        }
    });
    (port, mails)
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Append + broadcast exactly like the send route / provider stream do.
async fn emit(state: &AppState, sid: &str, kind: &str, data: Value) {
    let e = state.db.append_event(sid, kind, data).await.unwrap();
    state.broadcaster.broadcast(WsEvent {
        event_type: "event".into(),
        session_id: sid.into(),
        data: json!({
            "id": e.id, "seq": e.seq, "ts": e.ts, "kind": e.kind,
            "data": serde_json::from_str::<Value>(&e.data).unwrap(),
        }),
    });
}

async fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..200 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

fn paths(c: &Captured, prefix: &str) -> Vec<Value> {
    c.lock()
        .unwrap()
        .iter()
        .filter(|(p, _)| p.starts_with(prefix))
        .map(|(_, v)| v.clone())
        .collect()
}

#[tokio::test]
async fn voice_turns_reach_webhooks_and_unwatched_turns_reach_email() {
    // SAFETY: set before any thread reads it; this binary has one test.
    unsafe { std::env::set_var("PECKBOARD_MIRROR_TEST_ENDPOINTS", "1") };
    let (state, admin, user) = build_state().await;
    let (http, hooks) = spawn_receiver().await;
    let (smtp_port, mails) = spawn_smtp().await;
    let app = peckboard::routes::assistant::router(state.clone())
        .merge(peckboard::routes::voice::router(state.clone()))
        .with_state(state.clone());

    state
        .db
        .create_folder(NewFolder {
            id: "f1".into(),
            name: "f".into(),
            path: "/tmp".into(),
            created_at: "now".into(),
        })
        .await
        .unwrap();
    let now = chrono::Utc::now().to_rfc3339();
    let voice = state
        .db
        .create_session(NewSession {
            id: "voice-1".into(),
            name: "Assistant".into(),
            folder_id: "f1".into(),
            created_at: now.clone(),
            last_activity: now,
            is_expert: true,
            expert_kind: Some("voice".into()),
            is_permanent: true,
            user_id: Some("u1".into()),
            ..Default::default()
        })
        .await
        .unwrap();

    // ── settings API ──
    let (status, _) = call(&app, "GET", "/api/assistant/mirror", &user, Value::Null).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = call(
        &app,
        "PUT",
        "/api/assistant/mirror",
        &admin,
        json!({
            "slack": { "enabled": true, "webhook_url": "https://example.com/hook" },
            "discord": { "enabled": true },
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["errors"]["slack.webhook_url"].is_string(), "{body}");
    assert!(body["errors"]["discord.webhook_url"].is_string(), "{body}");

    let slack_url = format!("http://{http}/slack/T0/B0/secretpart");
    let discord_url = format!("http://{http}/discord/1/secretpart");
    let (status, body) = call(
        &app,
        "PUT",
        "/api/assistant/mirror",
        &admin,
        json!({
            "slack": { "enabled": true, "webhook_url": slack_url },
            "discord": { "enabled": true, "webhook_url": discord_url },
            "email": { "enabled": true, "to": "me@example.com" },
            "smtp": { "host": "127.0.0.1", "port": smtp_port, "tls": "none",
                      "from": "peckboard@example.com" },
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["slack"]["webhook_set"], true);
    assert_eq!(body["discord"]["status"]["state"], "never");
    assert_eq!(body["smtp"]["password_set"], false);
    assert_eq!(body["watched"], false);
    assert!(!body.to_string().contains("secretpart"), "{body}");

    AssistantMirror::of(&state)
        .await
        .set_digest_timings(Duration::from_millis(300), Duration::ZERO);

    // ── unwatched turn: webhooks + email digest ──
    emit(
        &state,
        &voice.id,
        "user",
        json!({ "text": format!("{INTERRUPT_MARKER}hello there"), "source": "voice-mic" }),
    )
    .await;
    emit(&state, &voice.id, "agent-start", json!({})).await;
    emit(
        &state,
        &voice.id,
        "agent-thinking",
        json!({ "text": "private" }),
    )
    .await;
    emit(
        &state,
        &voice.id,
        "agent-text",
        json!({ "text": "[Hi](/hˈI/) " }),
    )
    .await;
    emit(
        &state,
        &voice.id,
        "agent-text",
        json!({ "text": "there @everyone, ghp_abcdefghijklmnopqrstuvwxyz0123456789" }),
    )
    .await;
    emit(
        &state,
        &voice.id,
        "agent-end",
        json!({ "status": "complete" }),
    )
    .await;
    emit(
        &state,
        &voice.id,
        "user",
        json!({ "text": "[relay] update from dev: done", "source": "voice-relay" }),
    )
    .await;

    wait_for("first turn on both webhooks + digest", || {
        paths(&hooks, "/slack").len() >= 2
            && paths(&hooks, "/discord").len() >= 2
            && !mails.lock().unwrap().is_empty()
    })
    .await;
    let slack = paths(&hooks, "/slack");
    assert_eq!(slack[0]["text"], "You: hello there");
    let reply = slack[1]["text"].as_str().unwrap();
    assert!(
        reply.starts_with("Assistant: Hi there @everyone"),
        "{reply}"
    );
    assert!(
        !reply.contains("ghp_") && !reply.contains("private"),
        "{reply}"
    );
    let discord = paths(&hooks, "/discord");
    assert_eq!(discord[0]["content"], "You: hello there");
    assert_eq!(discord[1]["allowed_mentions"]["parse"], json!([]));
    let mail = mails.lock().unwrap()[0].clone();
    assert!(
        mail.contains("Subject: Peckboard Assistant: 2 new messages"),
        "{mail}"
    );
    assert!(mail.contains("hello there"), "{mail}");

    // ── watched turn: webhooks only ──
    let (status, _) = call(
        &app,
        "POST",
        "/api/voice/activity",
        &admin,
        json!({ "session_id": voice.id, "state": "panel_visible" }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    emit(
        &state,
        &voice.id,
        "user",
        json!({ "text": "second", "source": "voice-mic" }),
    )
    .await;
    emit(&state, &voice.id, "agent-text", json!({ "text": "Done." })).await;
    emit(&state, &voice.id, "agent-end", json!({})).await;
    wait_for("second turn on slack", || {
        paths(&hooks, "/slack").len() >= 4
    })
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(mails.lock().unwrap().len(), 1, "no digest while watched");
    // No relay turn was mirrored.
    assert_eq!(paths(&hooks, "/slack").len(), 4);

    let (_, body) = call(&app, "GET", "/api/assistant/mirror", &admin, Value::Null).await;
    assert_eq!(body["watched"], true);
    assert_eq!(body["slack"]["status"]["state"], "ok", "{body}");
    assert_eq!(body["email"]["status"]["state"], "ok", "{body}");

    // ── test endpoint ──
    let (status, body) = call(
        &app,
        "POST",
        "/api/assistant/mirror/test",
        &admin,
        json!({ "channel": "discord" }),
    )
    .await;
    assert_eq!((status, body["ok"].clone()), (StatusCode::OK, json!(true)));
    assert_eq!(paths(&hooks, "/discord").len(), 5);
    let (status, body) = call(
        &app,
        "POST",
        "/api/assistant/mirror/test",
        &admin,
        json!({ "channel": "email" }),
    )
    .await;
    assert_eq!(body["ok"], true, "{body}");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(mails.lock().unwrap().len(), 2);
    let (status, _) = call(
        &app,
        "POST",
        "/api/assistant/mirror/test",
        &admin,
        json!({ "channel": "pager" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // ── "Clear session": the log restarts at seq 1 and is still mirrored ──
    state.db.delete_events_by_session(&voice.id).await.unwrap();
    emit(
        &state,
        &voice.id,
        "user",
        json!({ "text": "after clear", "source": "voice-mic" }),
    )
    .await;
    wait_for("a turn after clear on slack", || {
        paths(&hooks, "/slack")
            .iter()
            .any(|p| p["text"] == "You: after clear")
    })
    .await;
}
