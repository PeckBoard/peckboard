//! Timed unlock of encrypted env vars over the HTTP surface:
//! `POST /api/env-vars/unlock` opens a window of the caller's choosing, which
//! answers any prompt a session is already blocked on and keeps later
//! dispatches from raising one at all (`warm_env_unlock_cache` skips on a
//! cache hit). `GET /api/env-vars/unlock-status` reports what's left.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use peckboard::auth::password::hash_password;
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewAuthSession, NewEnvVar, NewFolder, NewUser};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::service::env_vars::{EnvUnlockRegistry, encrypt_value, set_global_registry};
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::ws::broadcaster::Broadcaster;
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery staple";

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
    let provider_registry = Arc::new(ProviderRegistry::new());
    let push_service = PushService::new(&config.data_dir);

    let state = Arc::new(AppState {
        background: Default::default(),
        plugin_ws_tickets: Default::default(),
        device_registry: Default::default(),
        env_unlock: Arc::new(EnvUnlockRegistry::new()),
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
        provider_registry: provider_registry.clone(),
        session_manager: SessionManager::new(provider_registry),
        repeating_task_manager: peckboard::repeating::RepeatingTaskManager::new(),
        run_auditor: peckboard::repeating::RunAuditor::new(),
        mcp_tokens: McpTokenRegistry::new(),
        push_service,
        remote_access: peckboard::service::remote_access::RemoteAccess::inert(),
        terminals: peckboard::terminal::TerminalManager::inert(),
        tls: Arc::new(peckboard::state::TlsState::new()),
    });
    std::mem::forget(tmp);
    state
}

async fn mint_user(state: &AppState, user_id: &str) -> String {
    let ts = chrono::Utc::now().to_rfc3339();
    state
        .db
        .create_user(NewUser {
            id: user_id.into(),
            username: user_id.into(),
            email: None,
            password_hash: hash_password(PASSWORD).unwrap(),
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

/// Seal `value` under [`PASSWORD`] and store it as `owner`'s encrypted var.
async fn seed_encrypted_var(state: &AppState, id: &str, name: &str, value: &str, owner: &str) {
    let enc = encrypt_value(PASSWORD, value).unwrap();
    let ts = chrono::Utc::now().to_rfc3339();
    state
        .db
        .upsert_env_var(NewEnvVar {
            id: id.into(),
            name: name.into(),
            value: None,
            ciphertext: Some(enc.ciphertext_b64),
            nonce: Some(enc.nonce_hex),
            kdf_salt: Some(enc.kdf_salt_hex),
            encrypted: true,
            encrypted_by: Some(owner.into()),
            folder_id: None,
            created_at: ts.clone(),
            updated_at: ts,
        })
        .await
        .unwrap();
}

fn app(state: &Arc<AppState>) -> axum::Router {
    peckboard::routes::env_vars::router(state.clone()).with_state(state.clone())
}

fn post(jwt: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {jwt}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get(jwt: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {jwt}"))
        .body(Body::empty())
        .unwrap()
}

async fn json_body(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn unlock_opens_a_window_and_answers_a_waiting_prompt() {
    let state = build_state().await;
    let jwt = mint_user(&state, "u1").await;
    seed_encrypted_var(&state, "e1", "GITHUB_PAT", "ghp-token", "u1").await;

    // Locked to start with: a session would prompt.
    let status = json_body(
        app(&state)
            .oneshot(get(&jwt, "/api/env-vars/unlock-status"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status["unlocked"], serde_json::json!(false));
    assert_eq!(status["var_count"], serde_json::json!(1));

    // A session is already blocked on a prompt for these vars.
    let (_req_id, rx) = state
        .env_unlock
        .begin_request("u1", vec!["GITHUB_PAT".into()])
        .await;

    let resp = app(&state)
        .oneshot(post(
            &jwt,
            "/api/env-vars/unlock",
            serde_json::json!({ "password": PASSWORD, "duration": "15m" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;
    assert_eq!(body["unlocked"], serde_json::json!(1));
    // The waiting session gets the values from this unlock instead of sitting
    // on its dialog until the answer timeout.
    assert_eq!(body["resolved_prompts"], serde_json::json!(1));
    assert_eq!(
        rx.await.unwrap().unwrap().get("e1").map(String::as_str),
        Some("ghp-token")
    );

    // The window is the one that was asked for, not the 30-minute default.
    let left = body["expires_in_secs"].as_u64().unwrap();
    assert!(left > 14 * 60 && left <= 15 * 60, "expires_in_secs: {left}");

    // And a later dispatch finds the cache warm, so it never prompts.
    assert!(state.env_unlock.cache_get("u1").await.is_some());
    let status = json_body(
        app(&state)
            .oneshot(get(&jwt, "/api/env-vars/unlock-status"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status["unlocked"], serde_json::json!(true));
    assert!(status["expires_in_secs"].as_u64().unwrap() > 14 * 60);

    // Locking ends the window early.
    let resp = app(&state)
        .oneshot(post(&jwt, "/api/env-vars/lock", serde_json::json!({})))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(state.env_unlock.cache_get("u1").await.is_none());
}

#[tokio::test]
async fn until_lock_window_reports_no_expiry() {
    let state = build_state().await;
    let jwt = mint_user(&state, "u1").await;
    seed_encrypted_var(&state, "e1", "GITHUB_PAT", "ghp-token", "u1").await;

    let body = json_body(
        app(&state)
            .oneshot(post(
                &jwt,
                "/api/env-vars/unlock",
                serde_json::json!({ "password": PASSWORD, "duration": "until-lock" }),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(body["expires_in_secs"], serde_json::Value::Null);

    let status = json_body(
        app(&state)
            .oneshot(get(&jwt, "/api/env-vars/unlock-status"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status["unlocked"], serde_json::json!(true));
    assert_eq!(status["expires_in_secs"], serde_json::Value::Null);
}

#[tokio::test]
async fn bad_password_duration_or_nothing_to_unlock_leaves_it_locked() {
    let state = build_state().await;
    let jwt = mint_user(&state, "u1").await;

    // No encrypted vars: nothing decrypts, so nothing checked the password —
    // reporting success here would show an unlocked window backed by nothing.
    let resp = app(&state)
        .oneshot(post(
            &jwt,
            "/api/env-vars/unlock",
            serde_json::json!({ "password": PASSWORD, "duration": "1h" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    seed_encrypted_var(&state, "e1", "GITHUB_PAT", "ghp-token", "u1").await;

    let resp = app(&state)
        .oneshot(post(
            &jwt,
            "/api/env-vars/unlock",
            serde_json::json!({ "password": "wrong", "duration": "1h" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // An unknown window is rejected rather than falling back to some default
    // — a client can't widen the window by sending garbage.
    let resp = app(&state)
        .oneshot(post(
            &jwt,
            "/api/env-vars/unlock",
            serde_json::json!({ "password": PASSWORD, "duration": "forever" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    assert!(state.env_unlock.cache_get("u1").await.is_none());
}

/// Regression: a secret added (or edited) while its owner is unlocked used to
/// be missing from command env until the next unlock, because the cache was
/// a snapshot taken at unlock time and the upsert route never touched it.
#[tokio::test]
async fn secret_added_while_unlocked_reaches_folder_commands() {
    let state = build_state().await;
    // `command_env_blocking` reads the process-global registry; only this
    // test in the binary binds it.
    set_global_registry(state.env_unlock.clone());
    let jwt = mint_user(&state, "u1").await;
    seed_encrypted_var(&state, "e1", "GITHUB_PAT", "ghp-token", "u1").await;
    state
        .db
        .create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f1".into(),
            created_at: chrono::Utc::now().to_rfc3339(),
        })
        .await
        .unwrap();

    let resp = app(&state)
        .oneshot(post(
            &jwt,
            "/api/env-vars/unlock",
            serde_json::json!({ "password": PASSWORD, "duration": "1h" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Added while unlocked, scoped to the folder.
    let resp = app(&state)
        .oneshot(post(
            &jwt,
            "/api/env-vars",
            serde_json::json!({
                "name": "PB_FOLDER_SECRET",
                "value": "folder-secret-1",
                "encrypt": true,
                "password": PASSWORD,
                "folder_id": "f1",
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let folder_env = |db: peckboard::db::Db| async move {
        tokio::task::spawn_blocking(move || {
            peckboard::service::secret_mask::command_env_blocking(&db, Some("f1"))
                .0
                .into_iter()
                .collect::<std::collections::HashMap<_, _>>()
        })
        .await
        .unwrap()
    };
    let env = folder_env(state.db.clone()).await;
    assert_eq!(
        env.get("PB_FOLDER_SECRET").map(String::as_str),
        Some("folder-secret-1")
    );
    assert_eq!(env.get("GITHUB_PAT").map(String::as_str), Some("ghp-token"));

    // Editing it while unlocked replaces the cached plaintext.
    let resp = app(&state)
        .oneshot(post(
            &jwt,
            "/api/env-vars",
            serde_json::json!({
                "name": "PB_FOLDER_SECRET",
                "value": "folder-secret-2",
                "encrypt": true,
                "password": PASSWORD,
                "folder_id": "f1",
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let env = folder_env(state.db.clone()).await;
    assert_eq!(
        env.get("PB_FOLDER_SECRET").map(String::as_str),
        Some("folder-secret-2")
    );

    // Turning it plaintext drops the stale secret from the cache.
    let row = state
        .db
        .list_env_vars()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.name == "PB_FOLDER_SECRET")
        .unwrap();
    let resp = app(&state)
        .oneshot(post(
            &jwt,
            "/api/env-vars",
            serde_json::json!({
                "name": "PB_FOLDER_SECRET",
                "value": "plain",
                "folder_id": "f1",
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let cached = state.env_unlock.cache_get("u1").await.unwrap();
    assert!(!cached.contains_key(&row.id));
    assert!(cached.contains_key("e1"));
}

/// Save = unlock: saving a secret proves the password, so it unlocks ALL the
/// owner's secrets (not just the one saved) for the chosen window, and
/// answers a session already blocked on an unlock prompt.
#[tokio::test]
async fn saving_a_secret_unlocks_all_of_the_owners_secrets() {
    let state = build_state().await;
    let jwt = mint_user(&state, "u1").await;
    seed_encrypted_var(&state, "e1", "GITHUB_PAT", "ghp-token", "u1").await;
    assert!(state.env_unlock.cache_get("u1").await.is_none());
    // A session is already waiting on a prompt for this owner.
    let (_req, rx) = state
        .env_unlock
        .begin_request("u1", vec!["GITHUB_PAT".into()])
        .await;

    let resp = app(&state)
        .oneshot(post(
            &jwt,
            "/api/env-vars",
            serde_json::json!({
                "name": "PB_NEW_SECRET",
                "value": "new-secret",
                "encrypt": true,
                "password": PASSWORD,
                "unlock_duration": "15m",
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let cached = state.env_unlock.cache_get("u1").await.unwrap();
    // The OTHER secret is unlocked too, and so is the one just saved.
    assert_eq!(cached.get("e1").map(String::as_str), Some("ghp-token"));
    assert!(cached.values().any(|v| v == "new-secret"));
    // For the window asked for.
    let left = state
        .env_unlock
        .cache_status("u1")
        .await
        .unwrap()
        .expires_in_secs
        .unwrap();
    assert!(left > 14 * 60 && left <= 15 * 60, "left: {left}");
    // The waiting prompt was answered instead of timing out.
    assert!(rx.await.unwrap().is_some());

    // A later save with no window asked for keeps the open one's expiry.
    let resp = app(&state)
        .oneshot(post(
            &jwt,
            "/api/env-vars",
            serde_json::json!({
                "name": "PB_NEW_SECRET",
                "value": "new-secret-2",
                "encrypt": true,
                "password": PASSWORD,
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let left = state
        .env_unlock
        .cache_status("u1")
        .await
        .unwrap()
        .expires_in_secs
        .unwrap();
    assert!(left <= 15 * 60, "left: {left}");
}
