//! End-to-end test of the MCP-server OAuth flow against a mock provider.
//!
//! Boots a loopback "authorization server" implementing RFC 9728 protected-
//! resource metadata, RFC 8414 AS metadata, RFC 7591 dynamic client
//! registration, and a form-encoded token endpoint — then drives the real
//! pieces in order: `discover` → `register_client` → `begin_login` → the
//! public `GET /oauth/callback` route (code exchange + token store) →
//! `entries_for_provider_with_oauth` header injection → refresh on expiry.
//! A second test covers the Slack shape: static endpoints, no DCR, nested
//! `authed_user.access_token`, scopes via `user_scope=`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{Router, extract::Form, routing::get, routing::post};

use peckboard::auth::rate_limit::RateLimiter;
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::mcp_server::oauth;
use peckboard::service::mcp_server::user_servers::{McpOauthConfig, UserMcpServer};
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::ws::broadcaster::Broadcaster;

async fn build_state() -> Arc<AppState> {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();
    std::mem::forget(tmp);

    let registry = Arc::new(ProviderRegistry::new());
    let db = Db::in_memory().unwrap();
    let plugins = Arc::new(PluginManager::new(&data_dir, db.clone()));
    let session_manager = SessionManager::new(registry.clone()).with_plugins(plugins.clone());

    Arc::new(AppState {
        env_unlock: Arc::new(peckboard::service::env_vars::EnvUnlockRegistry::new()),
        config: Config {
            port: 0,
            https_port: 0,
            host: "127.0.0.1".into(),
            data_dir,
            mdns: false,
            keep_alive_hours: 0,
            provider_send_timeout_secs: 300,
        },
        db,
        plugins,
        builtin_plugins: Arc::new(BuiltinPluginRegistry::new()),
        jwt_secret: vec![0u8; 32],
        login_limiter: RateLimiter::new(100),
        password_change_limiter: RateLimiter::new(100),
        broadcaster: Broadcaster::new(),
        provider_registry: registry,
        session_manager,
        repeating_task_manager: peckboard::repeating::RepeatingTaskManager::new(),
        run_auditor: peckboard::repeating::RunAuditor::new(),
        mcp_tokens: McpTokenRegistry::new(),
        push_service: PushService::new(&std::env::temp_dir()),
    })
}

/// A mock MCP provider: resource metadata + AS metadata + DCR + token
/// endpoint. The token endpoint answers `authorization_code` with `at-1`
/// (+ refresh token) and `refresh_token` grants with `at-2`.
async fn serve_mock_provider() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");

    let b = base.clone();
    let prm = get(move || {
        let base = b.clone();
        async move {
            axum::Json(serde_json::json!({
                "resource": format!("{base}/mcp"),
                "authorization_servers": [base],
                "scopes_supported": ["read", "write"],
            }))
        }
    });
    let b = base.clone();
    let as_meta = get(move || {
        let base = b.clone();
        async move {
            axum::Json(serde_json::json!({
                "issuer": base,
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
                "registration_endpoint": format!("{base}/register"),
                "code_challenge_methods_supported": ["S256"],
            }))
        }
    });
    let register = post(
        |axum::Json(body): axum::Json<serde_json::Value>| async move {
            assert_eq!(body["token_endpoint_auth_method"], "none");
            let redirect = body["redirect_uris"][0].as_str().unwrap().to_string();
            assert!(redirect.ends_with("/oauth/callback"));
            axum::Json(serde_json::json!({ "client_id": "dyn-client-1" }))
        },
    );
    let token = post(|Form(form): Form<HashMap<String, String>>| async move {
        match form.get("grant_type").map(String::as_str) {
            Some("authorization_code") => {
                assert_eq!(form.get("code").map(String::as_str), Some("CODE123"));
                assert_eq!(
                    form.get("client_id").map(String::as_str),
                    Some("dyn-client-1")
                );
                assert!(form.get("code_verifier").is_some_and(|v| v.len() > 40));
                assert!(form.get("resource").is_some_and(|r| r.ends_with("/mcp")));
                axum::Json(serde_json::json!({
                    "access_token": "at-1",
                    "refresh_token": "rt-1",
                    "expires_in": 3600,
                }))
            }
            Some("refresh_token") => {
                assert_eq!(form.get("refresh_token").map(String::as_str), Some("rt-1"));
                axum::Json(serde_json::json!({
                    "access_token": "at-2",
                    "expires_in": 3600,
                }))
            }
            other => panic!("unexpected grant_type {other:?}"),
        }
    });

    let app = Router::new()
        .route("/.well-known/oauth-protected-resource/mcp", prm)
        .route("/.well-known/oauth-authorization-server", as_meta)
        .route("/register", register)
        .route("/token", token);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

/// Boot the real mcp_oauth router (public callback included).
async fn serve_peckboard(state: Arc<AppState>) -> String {
    let app = peckboard::routes::mcp_oauth::router(state.clone()).with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn query_param(url: &str, key: &str) -> Option<String> {
    url.split(['?', '&'])
        .find_map(|p| p.strip_prefix(&format!("{key}=")))
        .map(|v| urlencoding::decode(v).unwrap().into_owned())
}

fn oauth_server(id: &str, name: &str, url: String, cfg: Option<McpOauthConfig>) -> UserMcpServer {
    UserMcpServer {
        id: id.into(),
        name: name.into(),
        transport: "http".into(),
        command: String::new(),
        args: Vec::new(),
        env: Vec::new(),
        url,
        headers: Vec::new(),
        url_options: Vec::new(),
        auth: "oauth".into(),
        oauth: cfg,
        enabled: true,
        providers: Vec::new(),
        disabled_tools: Vec::new(),
    }
}

#[tokio::test]
async fn full_flow_discovery_dcr_callback_injection_refresh() {
    let provider = serve_mock_provider().await;
    let state = build_state().await;
    let pb = serve_peckboard(state.clone()).await;
    let http = oauth::http_client();

    let mcp_url = format!("{provider}/mcp");
    let server = oauth_server(
        "srv-1",
        "mocklinear",
        mcp_url.clone(),
        Some(McpOauthConfig::default()),
    );

    // Discovery walks PRM → AS metadata.
    let cfg = McpOauthConfig::default();
    let endpoints = oauth::discover(&http, &mcp_url, &cfg).await.unwrap();
    assert_eq!(endpoints.authorize_url, format!("{provider}/authorize"));
    assert_eq!(endpoints.token_url, format!("{provider}/token"));
    assert_eq!(endpoints.resource.as_deref(), Some(mcp_url.as_str()));
    assert_eq!(endpoints.scopes.as_deref(), Some("read write"));

    // DCR mints a client id.
    let redirect_uri = format!("{pb}/oauth/callback");
    let (client_id, secret) = oauth::register_client(
        &http,
        endpoints.registration_url.as_deref().unwrap(),
        &redirect_uri,
        endpoints.scopes.as_deref(),
    )
    .await
    .unwrap();
    assert_eq!(client_id, "dyn-client-1");
    assert!(secret.is_none());

    // Start the login; pull the state out of the authorize URL.
    let url = oauth::begin_login(&endpoints, &server, client_id, secret, redirect_uri);
    assert!(url.starts_with(&format!("{provider}/authorize?")));
    let st = query_param(&url, "state").unwrap();
    assert!(query_param(&url, "code_challenge").is_some());

    // The provider "redirects the browser" to our public callback.
    let resp = reqwest::get(format!("{pb}/oauth/callback?code=CODE123&state={st}"))
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let body = resp.text().await.unwrap();
    assert!(body.contains("Connected"), "callback page said: {body}");

    // Token stored under the server id; dispatch injects the header.
    let tokens = oauth::load_tokens(&state.db).await;
    assert_eq!(tokens.get("srv-1").unwrap().access_token, "at-1");
    assert_eq!(
        tokens.get("srv-1").unwrap().refresh_token.as_deref(),
        Some("rt-1")
    );

    let entries = peckboard::service::mcp_server::user_servers::entries_for_provider_with_oauth(
        &state.db,
        std::slice::from_ref(&server),
        "claude",
    )
    .await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].1["headers"]["Authorization"], "Bearer at-1");

    // Age the token into the refresh margin: next resolution refreshes and
    // keeps the un-rotated refresh token.
    let mut aged = tokens.get("srv-1").unwrap().clone();
    aged.expires_at_ms = Some(chrono::Utc::now().timestamp_millis() + 1000);
    oauth::put_token(&state.db, aged).await.unwrap();

    let bearer = oauth::bearer_for_server(&state.db, &server).await.unwrap();
    assert_eq!(bearer, "Bearer at-2");
    let tokens = oauth::load_tokens(&state.db).await;
    assert_eq!(tokens.get("srv-1").unwrap().access_token, "at-2");
    assert_eq!(
        tokens.get("srv-1").unwrap().refresh_token.as_deref(),
        Some("rt-1")
    );

    // A used state cannot be replayed.
    let resp = reqwest::get(format!("{pb}/oauth/callback?code=CODE123&state={st}"))
        .await
        .unwrap();
    assert!(resp.text().await.unwrap().contains("expired"));
}

#[tokio::test]
async fn slack_style_static_endpoints_nested_token_and_user_scope() {
    // Slack-shaped provider: no discovery documents, client_secret_post,
    // user token nested under authed_user.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let token = post(|Form(form): Form<HashMap<String, String>>| async move {
        assert_eq!(form.get("client_id").map(String::as_str), Some("slack-app"));
        assert_eq!(form.get("client_secret").map(String::as_str), Some("shh"));
        axum::Json(serde_json::json!({
            "ok": true,
            "authed_user": { "access_token": "xoxp-123" },
        }))
    });
    let app = Router::new().route("/api/oauth.v2.user.access", token);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let state = build_state().await;
    let pb = serve_peckboard(state.clone()).await;
    let http = oauth::http_client();

    let cfg = McpOauthConfig {
        authorize_url: Some(format!("{base}/oauth/v2_user/authorize")),
        token_url: Some(format!("{base}/api/oauth.v2.user.access")),
        client_id: Some("slack-app".into()),
        client_secret: Some("shh".into()),
        scopes: Some("search:read.public files:read".into()),
        scope_param: Some("user_scope".into()),
        token_field: Some("authed_user.access_token".into()),
        ..Default::default()
    };
    let server = oauth_server(
        "srv-slack",
        "slack",
        format!("{base}/mcp"),
        Some(cfg.clone()),
    );

    let endpoints = oauth::discover(&http, &server.url, &cfg).await.unwrap();
    let url = oauth::begin_login(
        &endpoints,
        &server,
        "slack-app".into(),
        Some("shh".into()),
        format!("{pb}/oauth/callback"),
    );
    assert!(url.contains("user_scope=search%3Aread.public%20files%3Aread"));
    assert!(!url.contains("&scope="));

    let st = query_param(&url, "state").unwrap();
    let resp = reqwest::get(format!("{pb}/oauth/callback?code=SLACKCODE&state={st}"))
        .await
        .unwrap();
    assert!(resp.text().await.unwrap().contains("Connected"));

    let tokens = oauth::load_tokens(&state.db).await;
    let tok = tokens.get("srv-slack").unwrap();
    assert_eq!(tok.access_token, "xoxp-123");
    assert_eq!(tok.expires_at_ms, None, "no expiry ⇒ long-lived");

    let bearer = oauth::bearer_for_server(&state.db, &server).await.unwrap();
    assert_eq!(bearer, "Bearer xoxp-123");
}
