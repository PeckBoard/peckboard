//! Regression tests for the admin gate on host-wide settings routes.
//!
//! The Claude permission mode (`--dangerously-skip-permissions` for every
//! project and every user on the host), the persistent `run_command` approval
//! list, the global MCP server list (whose probe route spawns a program named
//! in the request body) and the self-update / re-exec routes are all
//! host-wide. None of them are partitioned per user, so an admin-created
//! non-admin must get a 403 — while an admin keeps working.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewAuthSession, NewUser};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::routes::agent_vars::router as agent_vars_router;
use peckboard::routes::claude_accounts::router as claude_accounts_router;
use peckboard::routes::env_vars::router as env_vars_router;
use peckboard::routes::grok_accounts::router as grok_accounts_router;
use peckboard::routes::kimi_accounts::router as kimi_accounts_router;
use peckboard::routes::mcp_oauth::router as mcp_oauth_router;
use peckboard::routes::ollama::router as ollama_router;
use peckboard::routes::plugins::router as plugins_router;
use peckboard::routes::settings::router as settings_router;
use peckboard::routes::ssh_keys::router as ssh_keys_router;
use peckboard::routes::system_prompts::router as system_prompts_router;
use peckboard::routes::update::router as update_router;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::ws::broadcaster::Broadcaster;
use serde_json::Value;
use tower::ServiceExt;

struct Fixture {
    state: Arc<AppState>,
    admin_token: String,
    user_token: String,
}

/// Seed a user + auth session and mint a bearer token for it.
async fn seed_user(
    db: &Db,
    jwt_secret: &[u8],
    id: &str,
    username: &str,
    role: &str,
    auth_session_id: &str,
) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    db.create_user(NewUser {
        id: id.into(),
        username: username.into(),
        email: None,
        password_hash: "h".into(),
        role: role.into(),
        created_at: now.clone(),
        updated_at: now,
    })
    .await
    .unwrap();

    let (token, _exp) = create_token(jwt_secret, id, role, auth_session_id).unwrap();
    db.create_auth_session(NewAuthSession {
        id: auth_session_id.into(),
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

async fn build_fixture() -> Fixture {
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
    let ssh_vault_key =
        peckboard::service::ssh_keys::load_or_create_vault_key(&config.data_dir).unwrap();
    let mfa_vault_key =
        peckboard::auth::mfa::vault::load_or_create_vault_key(&config.data_dir).unwrap();
    let admin_token = seed_user(&db, &jwt_secret, "u1", "admin", "admin", "as1").await;
    let user_token = seed_user(&db, &jwt_secret, "u2", "tester2", "user", "as2").await;

    let state = Arc::new(AppState {
        env_unlock: Arc::new(peckboard::service::env_vars::EnvUnlockRegistry::new()),
        config,
        db,
        plugins,
        builtin_plugins: Arc::new(BuiltinPluginRegistry::new()),
        jwt_secret,
        password_change_limiter: RateLimiter::<String>::new(5),
        login_limiter: RateLimiter::new(60),
        ssh_vault_key,
        mfa_vault_key,
        broadcaster: Broadcaster::new(),
        provider_registry,
        session_manager,
        repeating_task_manager: peckboard::repeating::RepeatingTaskManager::new(),
        run_auditor: peckboard::repeating::RunAuditor::new(),
        mcp_tokens: McpTokenRegistry::new(),
        push_service,
        tls: Arc::new(peckboard::state::TlsState::new()),
    });
    // The data dir has to outlive the state; the process is short-lived.
    std::mem::forget(tmp);

    Fixture {
        state,
        admin_token,
        user_token,
    }
}

fn make_router(state: Arc<AppState>) -> axum::Router {
    settings_router(state.clone())
        .merge(update_router(state.clone()))
        .merge(plugins_router(state.clone()))
        .merge(claude_accounts_router(state.clone()))
        .merge(grok_accounts_router(state.clone()))
        .merge(kimi_accounts_router(state.clone()))
        .merge(ssh_keys_router(state.clone()))
        .merge(env_vars_router(state.clone()))
        .merge(agent_vars_router(state.clone()))
        .merge(system_prompts_router(state.clone()))
        .merge(ollama_router(state.clone()))
        .merge(mcp_oauth_router(state.clone()))
        .with_state(state)
}

/// Fire one request and return its status (plus the parsed JSON body, if any).
async fn call(
    state: Arc<AppState>,
    token: Option<&str>,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let req = match body {
        Some(v) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let resp = make_router(state).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

#[tokio::test]
async fn non_admin_cannot_flip_the_agent_permission_mode() {
    let f = build_fixture().await;

    let (status, body) = call(
        f.state.clone(),
        Some(&f.user_token),
        Method::PUT,
        "/api/settings/tool-permissions",
        Some(serde_json::json!({ "bypass": true })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a role=user JWT must not be able to bypass the host permission gate"
    );
    assert_eq!(body["error"], "admin only");

    // Reading it is admin-only too, and the write must not have landed.
    let (read_status, _) = call(
        f.state.clone(),
        Some(&f.user_token),
        Method::GET,
        "/api/settings/tool-permissions",
        None,
    )
    .await;
    assert_eq!(read_status, StatusCode::FORBIDDEN);

    let (admin_status, admin_body) = call(
        f.state.clone(),
        Some(&f.admin_token),
        Method::GET,
        "/api/settings/tool-permissions",
        None,
    )
    .await;
    assert_eq!(admin_status, StatusCode::OK);
    assert_eq!(
        admin_body["bypass"], false,
        "the rejected non-admin PUT must not have persisted"
    );
}

#[tokio::test]
async fn admin_can_still_set_the_agent_permission_mode() {
    let f = build_fixture().await;

    let (status, _) = call(
        f.state.clone(),
        Some(&f.admin_token),
        Method::PUT,
        "/api/settings/tool-permissions",
        Some(serde_json::json!({ "bypass": true })),
    )
    .await;
    assert!(
        status.is_success(),
        "admin PUT should succeed, got {status}"
    );

    let (_, body) = call(
        f.state.clone(),
        Some(&f.admin_token),
        Method::GET,
        "/api/settings/tool-permissions",
        None,
    )
    .await;
    assert_eq!(body["bypass"], true);

    // The pre-rename path is still served (a cached web bundle may use it)
    // and still admin-gated — same handler, same store key.
    let (alias_status, alias_body) = call(
        f.state.clone(),
        Some(&f.admin_token),
        Method::GET,
        "/api/settings/claude-permissions",
        None,
    )
    .await;
    assert_eq!(alias_status, StatusCode::OK);
    assert_eq!(alias_body["bypass"], true, "alias reads the same setting");

    let (alias_forbidden, _) = call(
        f.state.clone(),
        Some(&f.user_token),
        Method::PUT,
        "/api/settings/claude-permissions",
        Some(serde_json::json!({ "bypass": false })),
    )
    .await;
    assert_eq!(
        alias_forbidden,
        StatusCode::FORBIDDEN,
        "the deprecated alias must keep the admin gate"
    );
}

#[tokio::test]
async fn non_admin_cannot_reach_the_other_host_wide_settings_routes() {
    let f = build_fixture().await;

    let cases: Vec<(Method, &str, Option<Value>)> = vec![
        (Method::GET, "/api/settings/approved-commands", None),
        (Method::DELETE, "/api/settings/approved-commands/rm", None),
        (Method::GET, "/api/settings/mcp-servers", None),
        (
            Method::PUT,
            "/api/settings/mcp-servers",
            Some(serde_json::json!({ "servers": [] })),
        ),
        (
            Method::POST,
            "/api/settings/mcp-servers/probe",
            Some(serde_json::json!({
                "name": "pwn",
                "transport": "stdio",
                "command": "touch",
                "args": ["/tmp/peckboard-probe-should-not-run"],
            })),
        ),
        (
            Method::POST,
            "/api/settings/mcp-servers/check-command",
            Some(serde_json::json!({ "command": "npx" })),
        ),
    ];

    for (method, uri, body) in cases {
        let (status, _) = call(
            f.state.clone(),
            Some(&f.user_token),
            method.clone(),
            uri,
            body,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{method} {uri} must be admin only"
        );
    }

    // Settings that are not a security boundary stay open to every user.
    let (caveman, _) = call(
        f.state.clone(),
        Some(&f.user_token),
        Method::GET,
        "/api/settings/caveman",
        None,
    )
    .await;
    assert_eq!(caveman, StatusCode::OK);
}

#[tokio::test]
async fn non_admin_cannot_reach_the_update_routes() {
    let f = build_fixture().await;

    for (method, uri) in [
        (Method::GET, "/api/update/check"),
        (Method::POST, "/api/update/apply"),
    ] {
        let (status, body) = call(f.state.clone(), Some(&f.user_token), method, uri, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri} must be admin only");
        assert_eq!(body["error"], "admin only");
    }

    // An admin gets past the gate and into the handler. The handler itself
    // talks to the release CDN, so assert only that it is neither the auth
    // nor the admin rejection (offline runs surface 502 from the handler).
    let (admin_status, _) = call(
        f.state.clone(),
        Some(&f.admin_token),
        Method::GET,
        "/api/update/check",
        None,
    )
    .await;
    assert!(
        admin_status != StatusCode::FORBIDDEN && admin_status != StatusCode::UNAUTHORIZED,
        "admin must reach the update handler, got {admin_status}"
    );
}

#[tokio::test]
async fn unauthenticated_requests_are_rejected_before_the_admin_check() {
    let f = build_fixture().await;

    let (status, _) = call(
        f.state.clone(),
        None,
        Method::PUT,
        "/api/settings/claude-permissions",
        Some(serde_json::json!({ "bypass": true })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn non_admin_cannot_reach_the_plugin_management_routes() {
    let f = build_fixture().await;

    let cases: Vec<(Method, &str, Option<Value>)> = vec![
        (Method::DELETE, "/api/plugins/ollama", None),
        (
            Method::PUT,
            "/api/plugins/ollama/settings",
            Some(serde_json::json!({ "updates": {} })),
        ),
        (
            Method::POST,
            "/api/plugins/ollama/approval",
            Some(serde_json::json!({ "decision": "approve" })),
        ),
        (
            Method::POST,
            "/api/plugins/repositories",
            Some(serde_json::json!({ "label": "x", "url": "https://example.com" })),
        ),
        (
            Method::POST,
            "/api/plugins/registry/install",
            Some(serde_json::json!({ "id": "x" })),
        ),
    ];
    for (method, uri, body) in cases {
        let (status, _) = call(
            f.state.clone(),
            Some(&f.user_token),
            method.clone(),
            uri,
            body,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{method} {uri} must be admin only"
        );
    }

    // Reads stay open to any authenticated user.
    let (list_status, _) = call(
        f.state.clone(),
        Some(&f.user_token),
        Method::GET,
        "/api/plugins",
        None,
    )
    .await;
    assert_eq!(list_status, StatusCode::OK);
}

#[tokio::test]
async fn non_admin_cannot_manage_provider_accounts() {
    let f = build_fixture().await;

    for (create_uri, id_uri) in [
        ("/api/claude-accounts", "/api/claude-accounts/acc_x"),
        ("/api/grok-accounts", "/api/grok-accounts/acc_x"),
        ("/api/kimi-accounts", "/api/kimi-accounts/acc_x"),
    ] {
        let (create_status, body) = call(
            f.state.clone(),
            Some(&f.user_token),
            Method::POST,
            create_uri,
            Some(serde_json::json!({ "name": "probe" })),
        )
        .await;
        assert_eq!(
            create_status,
            StatusCode::FORBIDDEN,
            "POST {create_uri} must be admin only"
        );
        assert_eq!(body["error"], "admin only");

        let (delete_status, _) = call(
            f.state.clone(),
            Some(&f.user_token),
            Method::DELETE,
            id_uri,
            None,
        )
        .await;
        assert_eq!(
            delete_status,
            StatusCode::FORBIDDEN,
            "DELETE {id_uri} must be admin only"
        );
    }

    // Listing accounts stays open (the model picker needs it).
    let (list_status, _) = call(
        f.state.clone(),
        Some(&f.user_token),
        Method::GET,
        "/api/claude-accounts",
        None,
    )
    .await;
    assert_eq!(list_status, StatusCode::OK);
}

#[tokio::test]
async fn non_admin_cannot_write_env_or_agent_vars() {
    let f = build_fixture().await;

    for (upsert_uri, delete_uri) in [
        ("/api/env-vars", "/api/env-vars/v1"),
        ("/api/agent-vars", "/api/agent-vars/v1"),
    ] {
        let (upsert_status, _) = call(
            f.state.clone(),
            Some(&f.user_token),
            Method::POST,
            upsert_uri,
            Some(serde_json::json!({ "name": "FOO", "value": "bar" })),
        )
        .await;
        assert_eq!(
            upsert_status,
            StatusCode::FORBIDDEN,
            "POST {upsert_uri} must be admin only"
        );

        let (delete_status, _) = call(
            f.state.clone(),
            Some(&f.user_token),
            Method::DELETE,
            delete_uri,
            None,
        )
        .await;
        assert_eq!(
            delete_status,
            StatusCode::FORBIDDEN,
            "DELETE {delete_uri} must be admin only"
        );
    }

    // Listing stays open.
    let (list_status, _) = call(
        f.state.clone(),
        Some(&f.user_token),
        Method::GET,
        "/api/env-vars",
        None,
    )
    .await;
    assert_eq!(list_status, StatusCode::OK);
}

#[tokio::test]
async fn non_admin_cannot_manage_system_prompts() {
    let f = build_fixture().await;

    let (create_status, _) = call(
        f.state.clone(),
        Some(&f.user_token),
        Method::POST,
        "/api/system-prompts",
        Some(serde_json::json!({ "name": "probe", "body": "be terse" })),
    )
    .await;
    assert_eq!(create_status, StatusCode::FORBIDDEN);

    let (update_status, _) = call(
        f.state.clone(),
        Some(&f.user_token),
        Method::PUT,
        "/api/system-prompts/p1",
        Some(serde_json::json!({ "name": "probe", "body": "be terse" })),
    )
    .await;
    assert_eq!(update_status, StatusCode::FORBIDDEN);

    let (delete_status, _) = call(
        f.state.clone(),
        Some(&f.user_token),
        Method::DELETE,
        "/api/system-prompts/p1",
        None,
    )
    .await;
    assert_eq!(delete_status, StatusCode::FORBIDDEN);

    // Listing stays open (feeds the session-creation dropdown).
    let (list_status, _) = call(
        f.state.clone(),
        Some(&f.user_token),
        Method::GET,
        "/api/system-prompts",
        None,
    )
    .await;
    assert_eq!(list_status, StatusCode::OK);
}

#[tokio::test]
async fn non_admin_cannot_pull_ollama_models_or_disconnect_mcp_oauth() {
    let f = build_fixture().await;

    let (pull_status, _) = call(
        f.state.clone(),
        Some(&f.user_token),
        Method::POST,
        "/api/ollama/pull",
        Some(serde_json::json!({ "model": "llama3.2" })),
    )
    .await;
    assert_eq!(pull_status, StatusCode::FORBIDDEN);

    let (disconnect_status, _) = call(
        f.state.clone(),
        Some(&f.user_token),
        Method::DELETE,
        "/api/mcp-oauth/tokens/srv1",
        None,
    )
    .await;
    assert_eq!(disconnect_status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn non_admin_cannot_import_generate_rename_or_delete_ssh_keys() {
    let f = build_fixture().await;

    let (import_status, _) = call(
        f.state.clone(),
        Some(&f.user_token),
        Method::POST,
        "/api/ssh-keys",
        Some(serde_json::json!({ "name": "probe", "private_key": "not a key" })),
    )
    .await;
    assert_eq!(import_status, StatusCode::FORBIDDEN);

    let (generate_status, _) = call(
        f.state.clone(),
        Some(&f.user_token),
        Method::POST,
        "/api/ssh-keys/generate",
        Some(serde_json::json!({ "name": "probe" })),
    )
    .await;
    assert_eq!(generate_status, StatusCode::FORBIDDEN);

    let (rename_status, _) = call(
        f.state.clone(),
        Some(&f.user_token),
        Method::PATCH,
        "/api/ssh-keys/k1",
        Some(serde_json::json!({ "name": "renamed" })),
    )
    .await;
    assert_eq!(rename_status, StatusCode::FORBIDDEN);

    let (delete_status, _) = call(
        f.state.clone(),
        Some(&f.user_token),
        Method::DELETE,
        "/api/ssh-keys/k1",
        None,
    )
    .await;
    assert_eq!(delete_status, StatusCode::FORBIDDEN);

    // Listing and reading a public key stay open to any authenticated user.
    let (list_status, _) = call(
        f.state.clone(),
        Some(&f.user_token),
        Method::GET,
        "/api/ssh-keys",
        None,
    )
    .await;
    assert_eq!(list_status, StatusCode::OK);

    let (public_status, _) = call(
        f.state.clone(),
        Some(&f.user_token),
        Method::GET,
        "/api/ssh-keys/k1/public",
        None,
    )
    .await;
    assert_eq!(
        public_status,
        StatusCode::NOT_FOUND,
        "no key was created, but the route itself must not be admin-gated"
    );

    // An admin can actually generate one.
    let (admin_generate_status, body) = call(
        f.state.clone(),
        Some(&f.admin_token),
        Method::POST,
        "/api/ssh-keys/generate",
        Some(serde_json::json!({ "name": "admin-generated" })),
    )
    .await;
    assert_eq!(admin_generate_status, StatusCode::OK);
    assert_eq!(body["key_type"], "ed25519");
    assert!(
        body["public_key"]
            .as_str()
            .unwrap()
            .starts_with("ssh-ed25519 ")
    );
}
