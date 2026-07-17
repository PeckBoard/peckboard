//! Sudo askpass round-trip over the HTTP surface: the helper's token-gated
//! `POST /api/askpass` long-poll is resolved by the JWT'd
//! `POST /api/sessions/{id}/askpass-answer`, and the password travels back
//! as the response body (and nowhere else).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewAuthSession, NewUser};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::service::askpass::{AskpassEnv, AskpassRegistry};
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::ws::broadcaster::Broadcaster;
use tower::ServiceExt;

async fn build_state() -> (Arc<AppState>, AskpassRegistry) {
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
    let askpass = AskpassRegistry::new();
    let session_manager =
        SessionManager::new(provider_registry.clone()).with_askpass(Some(AskpassEnv {
            registry: askpass.clone(),
            script_path: "/tmp/askpass.sh".into(),
            url: "http://127.0.0.1:0/api/askpass".into(),
        }));
    let push_service = PushService::new(&config.data_dir);

    let state = Arc::new(AppState {
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
    });
    std::mem::forget(tmp);
    (state, askpass)
}

async fn mint_user(state: &AppState, user_id: &str) -> String {
    let ts = chrono::Utc::now().to_rfc3339();
    state
        .db
        .create_user(NewUser {
            id: user_id.into(),
            username: user_id.into(),
            email: None,
            password_hash: "h".into(),
            role: "admin".into(),
            created_at: ts.clone(),
            updated_at: ts,
        })
        .await
        .unwrap();
    let auth_session_id = format!("as-{user_id}");
    let (token, _exp) =
        create_token(&state.jwt_secret, user_id, "admin", &auth_session_id).unwrap();
    let now_secs = 1_000_000i64;
    state
        .db
        .create_auth_session(NewAuthSession {
            id: auth_session_id,
            user_id: user_id.into(),
            token_hash: hash_token(&token),
            created_at: now_secs,
            expires_at: now_secs + 7 * 24 * 60 * 60,
            user_agent: None,
            ip_address: None,
        })
        .await
        .unwrap();
    token
}

fn app(state: &Arc<AppState>) -> axum::Router {
    peckboard::routes::askpass::router(state.clone()).with_state(state.clone())
}

fn helper_req(token: &str, prompt: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/askpass")
        .header("X-Peckboard-Askpass-Token", token)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!("prompt={}", urlencoding_encode(prompt))))
        .unwrap()
}

/// Tiny local form-encoder — enough for test prompts.
fn urlencoding_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b' ' => "+".into(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn answer_req(jwt: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/sessions/sess-1/askpass-answer")
        .header(header::AUTHORIZATION, format!("Bearer {jwt}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).to_string()
}

#[tokio::test]
async fn password_round_trip() {
    let (state, askpass) = build_state().await;
    let jwt = mint_user(&state, "u1").await;
    let token = askpass.issue_token("sess-1").await;

    // Subscribe BEFORE the helper posts — a broadcast receiver only sees
    // events sent after it exists.
    let mut rx = state.broadcaster.subscribe_all();

    // The helper's long-poll runs concurrently with the UI answering.
    let helper_state = state.clone();
    let helper = tokio::spawn(async move {
        let resp = app(&helper_state)
            .oneshot(helper_req(&token, "[sudo] password for user:"))
            .await
            .unwrap();
        (resp.status(), body_string(resp).await)
    });

    // Grab the request_id from the broadcast the route emits when the
    // pending request is registered.
    let request_id = loop {
        let ev = rx.recv().await.unwrap();
        if ev.event_type == "askpass-request" {
            assert_eq!(ev.session_id, "sess-1");
            assert_eq!(
                ev.data.get("prompt").and_then(|v| v.as_str()),
                Some("[sudo] password for user:")
            );
            break ev
                .data
                .get("request_id")
                .and_then(|v| v.as_str())
                .unwrap()
                .to_string();
        }
    };

    let resp = app(&state)
        .oneshot(answer_req(
            &jwt,
            serde_json::json!({ "request_id": request_id, "password": "hunter2" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let (status, body) = helper.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "hunter2");

    // The same request can't be answered twice.
    let resp = app(&state)
        .oneshot(answer_req(
            &jwt,
            serde_json::json!({ "request_id": request_id, "password": "again" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::GONE);
}

#[tokio::test]
async fn cancel_rejects_the_helper() {
    let (state, askpass) = build_state().await;
    let jwt = mint_user(&state, "u1").await;
    let token = askpass.issue_token("sess-1").await;

    let mut rx = state.broadcaster.subscribe_all();
    let helper_state = state.clone();
    let helper = tokio::spawn(async move {
        let resp = app(&helper_state)
            .oneshot(helper_req(&token, "pw:"))
            .await
            .unwrap();
        resp.status()
    });
    let request_id = loop {
        let ev = rx.recv().await.unwrap();
        if ev.event_type == "askpass-request" {
            break ev.data["request_id"].as_str().unwrap().to_string();
        }
    };

    let resp = app(&state)
        .oneshot(answer_req(
            &jwt,
            serde_json::json!({ "request_id": request_id, "cancel": true }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(helper.await.unwrap(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn bad_tokens_and_auth_are_rejected() {
    let (state, _askpass) = build_state().await;
    let jwt = mint_user(&state, "u1").await;

    // Unknown helper token → 401.
    let resp = app(&state)
        .oneshot(helper_req("not-a-token", "pw:"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Answer without a JWT → 401 from require_auth.
    let req = Request::builder()
        .method("POST")
        .uri("/api/sessions/sess-1/askpass-answer")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({ "request_id": "x", "password": "p" }).to_string(),
        ))
        .unwrap();
    let resp = app(&state).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Authenticated answer for a request that never existed → 410.
    let resp = app(&state)
        .oneshot(answer_req(
            &jwt,
            serde_json::json!({ "request_id": "nope", "password": "p" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::GONE);
}
