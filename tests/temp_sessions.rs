//! Temp sessions: a session created with `is_temp` is deleted outright —
//! full cleanup + `session-deleted` broadcast — when the LAST tab pointing
//! at it (across all users) is closed via DELETE /api/me/tabs. Regular
//! sessions just lose the tab row. "Keep session" (PATCH is_temp=false)
//! opts a temp session out of the auto-delete before the close.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewAuthSession, NewFolder, NewUser};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::ws::broadcaster::Broadcaster;
use tower::ServiceExt;

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

    let ts = chrono::Utc::now().to_rfc3339();
    state
        .db
        .create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts,
        })
        .await
        .unwrap();

    state
}

/// Create a user + auth session and mint a bearer token for them.
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

/// Sessions + me routers merged — the temp flow spans both surfaces.
fn app(state: &Arc<AppState>) -> axum::Router {
    peckboard::routes::sessions::router(state.clone())
        .merge(peckboard::routes::me::router(state.clone()))
        .with_state(state.clone())
}

async fn send(state: &Arc<AppState>, req: Request<Body>) -> (StatusCode, serde_json::Value) {
    let resp = app(state).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

fn json_req(method: &str, uri: &str, token: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn empty_req(method: &str, uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

/// POST /api/sessions and return the created session id.
async fn create_session(state: &Arc<AppState>, token: &str, is_temp: bool) -> String {
    let (status, created) = send(
        state,
        json_req(
            "POST",
            "/api/sessions",
            token,
            serde_json::json!({ "name": "s", "folder_id": "f1", "is_temp": is_temp }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["is_temp"], serde_json::Value::Bool(is_temp));
    created["id"].as_str().unwrap().to_string()
}

async fn open_tab(state: &Arc<AppState>, token: &str, id: &str) -> serde_json::Value {
    let (status, body) = send(
        state,
        json_req(
            "POST",
            "/api/me/tabs",
            token,
            serde_json::json!({ "item_type": "session", "item_id": id }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    body
}

async fn close_tab(state: &Arc<AppState>, token: &str, id: &str) {
    let (status, _) = send(
        state,
        empty_req("DELETE", &format!("/api/me/tabs/session/{id}"), token),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn closing_last_tab_deletes_temp_session_and_broadcasts() {
    let state = build_state().await;
    let token = mint_user(&state, "u1").await;

    let id = create_session(&state, &token, true).await;

    // The tab strip learns the flag from the denormalized upsert response.
    let tab = open_tab(&state, &token, &id).await;
    assert_eq!(tab["is_temp"], serde_json::Value::Bool(true));

    let mut rx = state.broadcaster.subscribe_all();
    close_tab(&state, &token, &id).await;

    assert!(
        state.db.get_session(&id).await.unwrap().is_none(),
        "temp session must be deleted with its last tab",
    );

    // Other devices learn about it the same way as an explicit delete.
    let mut found = false;
    for _ in 0..8 {
        match tokio::time::timeout(std::time::Duration::from_millis(250), rx.recv()).await {
            Ok(Ok(event)) => {
                if event.event_type == "session-deleted" && event.session_id == id {
                    found = true;
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(
        found,
        "last-tab close of a temp session must broadcast session-deleted",
    );
}

#[tokio::test]
async fn closing_tab_keeps_regular_session() {
    let state = build_state().await;
    let token = mint_user(&state, "u1").await;

    let id = create_session(&state, &token, false).await;
    let tab = open_tab(&state, &token, &id).await;
    assert_eq!(tab["is_temp"], serde_json::Value::Bool(false));

    close_tab(&state, &token, &id).await;

    assert!(
        state.db.get_session(&id).await.unwrap().is_some(),
        "closing a tab must not delete a regular session",
    );
    assert_eq!(
        state
            .db
            .count_user_tabs_for_item("session", &id)
            .await
            .unwrap(),
        0,
        "the tab row itself must still be removed",
    );
}

#[tokio::test]
async fn second_user_tab_defers_temp_delete_until_last_close() {
    let state = build_state().await;
    let token_a = mint_user(&state, "ua").await;
    let token_b = mint_user(&state, "ub").await;

    let id = create_session(&state, &token_a, true).await;
    open_tab(&state, &token_a, &id).await;
    open_tab(&state, &token_b, &id).await;

    close_tab(&state, &token_a, &id).await;
    assert!(
        state.db.get_session(&id).await.unwrap().is_some(),
        "temp session must survive while another user's tab is open",
    );

    close_tab(&state, &token_b, &id).await;
    assert!(
        state.db.get_session(&id).await.unwrap().is_none(),
        "temp session must be deleted when the last tab closes",
    );
}

#[tokio::test]
async fn keep_session_clears_temp_and_survives_tab_close() {
    let state = build_state().await;
    let token = mint_user(&state, "u1").await;

    let id = create_session(&state, &token, true).await;
    open_tab(&state, &token, &id).await;

    // "Keep session" — the escape hatch the UI offers on temp tabs.
    let (status, patched) = send(
        &state,
        json_req(
            "PATCH",
            &format!("/api/sessions/{id}"),
            &token,
            serde_json::json!({ "is_temp": false }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(patched["is_temp"], serde_json::Value::Bool(false));

    close_tab(&state, &token, &id).await;
    assert!(
        state.db.get_session(&id).await.unwrap().is_some(),
        "a kept session must outlive its tab",
    );
}
