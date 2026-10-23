//! OAuth 2.1 `client_credentials` for Peckboard's own MCP server, end to
//! end through the real routers: the admin API mints a client, the token
//! endpoint swaps its secret for a bearer (HTTP Basic and post body), and
//! `/mcp` serves that bearer as the client's sealed backing session — with
//! the agent-process tools gated off, the folder/project boundary enforced,
//! and the network policy (loopback or HTTPS, never plain remote HTTP)
//! applied. Rotation and revocation kill old credentials.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use base64::Engine;
use peckboard::auth::rate_limit::RateLimiter;
use peckboard::auth::token::{create_token, generate_jwt_secret, hash_token};
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewAuthSession, NewFolder, NewProject, NewUser};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::service::mcp_server::McpTokenRegistry;
use peckboard::service::oauth_server::HttpsListener;
use peckboard::service::push::PushService;
use peckboard::state::AppState;
use peckboard::ws::broadcaster::Broadcaster;
use serde_json::{Value, json};
use tower::ServiceExt;

const HTTPS_PORT: u16 = 3345;

async fn build_state() -> (Arc<AppState>, String) {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();
    std::mem::forget(tmp);
    let db = Db::in_memory().unwrap();
    let jwt_secret = generate_jwt_secret();
    let now = chrono::Utc::now().to_rfc3339();

    db.create_user(NewUser {
        id: "u1".into(),
        username: "admin".into(),
        email: None,
        password_hash: "h".into(),
        role: "admin".into(),
        created_at: now.clone(),
        updated_at: now.clone(),
    })
    .await
    .unwrap();
    let (admin_jwt, _) = create_token(&jwt_secret, "u1", "admin", "as1").unwrap();
    db.create_auth_session(NewAuthSession {
        id: "as1".into(),
        user_id: "u1".into(),
        token_hash: hash_token(&admin_jwt),
        created_at: 1_000_000,
        expires_at: 1_000_000 + 7 * 24 * 60 * 60,
        user_agent: None,
        ip_address: None,
    })
    .await
    .unwrap();

    for (folder, project) in [("f1", "p1"), ("f2", "p2")] {
        db.create_folder(NewFolder {
            id: folder.into(),
            name: format!("Folder {folder}"),
            path: format!("/tmp/{folder}"),
            created_at: now.clone(),
        })
        .await
        .unwrap();
        db.create_project(NewProject {
            id: project.into(),
            name: format!("Project {project}"),
            context: String::new(),
            folder_id: folder.into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "task".into(),
            model: None,
            effort: None,
            budget_usd_cents: None,
            budget_period: None,
            worktree_isolation: true,
            parallel_instructions: false,
            auto_notify_changes: true,
            worker_communication: false,
            created_at: now.clone(),
            last_accessed_at: now.clone(),
        })
        .await
        .unwrap();
    }

    let registry = Arc::new(ProviderRegistry::new());
    let plugins = Arc::new(PluginManager::new(&data_dir, db.clone()));
    let session_manager = SessionManager::new(registry.clone()).with_plugins(plugins.clone());
    let state = Arc::new(AppState {
        background: Default::default(),
        plugin_ws_tickets: Default::default(),
        device_registry: Default::default(),
        env_unlock: Arc::new(peckboard::service::env_vars::EnvUnlockRegistry::new()),
        config: Config {
            port: 3344,
            https_port: HTTPS_PORT,
            host: "127.0.0.1".into(),
            data_dir,
            mdns: false,
            keep_alive_hours: 0,
            provider_send_timeout_secs: 300,
        },
        db,
        plugins,
        builtin_plugins: Arc::new(BuiltinPluginRegistry::new()),
        jwt_secret,
        ssh_vault_key: vec![0u8; 32],
        mfa_vault_key: vec![0u8; 32],
        login_limiter: RateLimiter::new(100),
        password_change_limiter: RateLimiter::new(100),
        broadcaster: Broadcaster::new(),
        provider_registry: registry,
        session_manager,
        repeating_task_manager: peckboard::repeating::RepeatingTaskManager::new(),
        run_auditor: peckboard::repeating::RunAuditor::new(),
        mcp_tokens: McpTokenRegistry::new(),
        tls: Arc::new(peckboard::state::TlsState::new()),
        push_service: PushService::new(&std::env::temp_dir()),
        remote_access: peckboard::service::remote_access::RemoteAccess::inert(),
        terminals: peckboard::terminal::TerminalManager::inert(),
    });
    (state, admin_jwt)
}

/// The `/mcp` + OAuth routers as a request from `peer` sees them, optionally
/// as served by the HTTPS listener.
fn app(state: &Arc<AppState>, peer: &str, https: bool) -> Router {
    let router = peckboard::routes::mcp::router(state.clone())
        .merge(peckboard::routes::oauth_server::router(state.clone()))
        .with_state(state.clone())
        .layer(MockConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    if https {
        router.layer(axum::Extension(HttpsListener))
    } else {
        router
    }
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, body)
}

fn admin(method: Method, uri: &str, jwt: &str, body: Option<Value>) -> Request<Body> {
    let b = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "peck.example:3344")
        .header(header::AUTHORIZATION, format!("Bearer {jwt}"));
    match body {
        Some(v) => b
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    }
}

fn token_basic(id: &str, secret: &str) -> Request<Body> {
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{id}:{secret}"));
    Request::builder()
        .method(Method::POST)
        .uri("/oauth/token")
        .header(header::AUTHORIZATION, format!("Basic {basic}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("grant_type=client_credentials&scope=mcp"))
        .unwrap()
}

fn token_post(body: Value) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri("/oauth/token")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn rpc(bearer: Option<&str>, method: &str, params: Value) -> Request<Body> {
    let mut b = Request::builder()
        .method(Method::POST)
        .uri("/mcp")
        .header(header::HOST, "peck.example:3345")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(t) = bearer {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    b.body(Body::from(
        json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string(),
    ))
    .unwrap()
}

fn tool_call(bearer: &str, name: &str, args: Value) -> Request<Body> {
    rpc(
        Some(bearer),
        "tools/call",
        json!({ "name": name, "arguments": args }),
    )
}

#[tokio::test]
async fn oauth_client_credentials_end_to_end() {
    let (state, jwt) = build_state().await;
    let local = app(&state, "127.0.0.1:50000", false);

    // ── Admin API: validation, create, list, endpoints ──
    let (s, _, _) = send(
        &local,
        admin(
            Method::POST,
            "/api/oauth-clients",
            &jwt,
            Some(json!({ "name": " ", "folder_id": "f1" })),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _, _) = send(
        &local,
        admin(
            Method::POST,
            "/api/oauth-clients",
            &jwt,
            Some(json!({ "name": "CI", "folder_id": "f1", "project_id": "p2" })),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "project outside the folder");
    let (s, _, _) = send(
        &local,
        admin(
            Method::POST,
            "/api/oauth-clients",
            &jwt,
            Some(json!({ "name": "CI", "folder_id": "nope" })),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "unknown folder");

    let (s, _, created) = send(
        &local,
        admin(
            Method::POST,
            "/api/oauth-clients",
            &jwt,
            Some(json!({ "name": "CI", "folder_id": "f1" })),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{created}");
    let row_id = created["client"]["id"].as_str().unwrap().to_string();
    let client_id = created["client"]["client_id"].as_str().unwrap().to_string();
    let secret = created["client_secret"].as_str().unwrap().to_string();
    let session_id = created["client"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(client_id.starts_with("pbc_") && secret.starts_with("pbcs_"));
    assert_eq!(created["client"]["folder_name"], "Folder f1");
    assert!(created["client"]["project_id"].is_null());

    // The backing session is sealed: nothing may ever dispatch an agent.
    let session = state.db.get_session(&session_id).await.unwrap().unwrap();
    assert_eq!(session.expert_kind.as_deref(), Some("external_mcp"));
    assert!(session.sealed_at.is_some());
    assert!(
        peckboard::provider::manager::ensure_not_sealed(&state.db, &session_id)
            .await
            .is_err()
    );

    let (s, _, list) = send(&local, admin(Method::GET, "/api/oauth-clients", &jwt, None)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["client_id"], client_id.as_str());
    assert!(list[0].get("secret_hash").is_none());

    let (s, _, ep) = send(
        &local,
        admin(Method::GET, "/api/oauth-clients/endpoints", &jwt, None),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(ep["token_url"], "https://peck.example:3345/oauth/token");
    assert_eq!(ep["mcp_url"], "https://peck.example:3345/mcp");
    assert_eq!(ep["https_port"], HTTPS_PORT);

    // ── Token endpoint ──
    let (s, h, tok) = send(&local, token_basic(&client_id, &secret)).await;
    assert_eq!(s, StatusCode::OK, "{tok}");
    assert_eq!(h[header::CACHE_CONTROL], "no-store");
    assert_eq!(tok["token_type"], "Bearer");
    assert_eq!(tok["expires_in"], 3600);
    assert_eq!(tok["scope"], "mcp");
    let basic_token = tok["access_token"].as_str().unwrap().to_string();
    assert!(basic_token.starts_with("pbat_"));

    let (s, _, tok) = send(
        &local,
        token_post(json!({
            "grant_type": "client_credentials",
            "client_id": client_id,
            "client_secret": secret,
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{tok}");
    let bearer = tok["access_token"].as_str().unwrap().to_string();

    let (s, _, err) = send(
        &local,
        token_post(
            json!({ "grant_type": "password", "client_id": client_id, "client_secret": secret }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(err["error"], "unsupported_grant_type");

    let (s, h, err) = send(&local, token_basic(&client_id, "pbcs_wrong")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(err["error"], "invalid_client");
    assert!(
        h[header::WWW_AUTHENTICATE]
            .to_str()
            .unwrap()
            .starts_with("Basic")
    );

    // ── /mcp as the client ──
    let (s, _, body) = send(&local, rpc(Some(&bearer), "tools/list", json!({}))).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let names: Vec<&str> = body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"list_cards"), "{names:?}");
    for hidden in ["spawn_subagent", "browser_open", "run_background"] {
        assert!(!names.contains(&hidden), "{hidden} advertised: {names:?}");
    }

    let (s, _, body) = send(
        &local,
        tool_call(&bearer, "list_cards", json!({ "project_id": "p1" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(body["result"]["content"].is_array(), "{body}");

    let (_, _, body) = send(
        &local,
        tool_call(&bearer, "list_cards", json!({ "project_id": "p2" })),
    )
    .await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not found"),
        "foreign-folder project reachable: {body}"
    );
    let (_, _, body) = send(
        &local,
        tool_call(&bearer, "spawn_subagent", json!({ "prompt": "hi" })),
    )
    .await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("blocked"),
        "{body}"
    );

    // ── Network policy ──
    let remote_http = app(&state, "203.0.113.7:40000", false);
    let (s, _, _) = send(&remote_http, rpc(Some(&bearer), "tools/list", json!({}))).await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "plain remote HTTP must be refused"
    );
    let (s, _, _) = send(&remote_http, token_basic(&client_id, &secret)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    let remote_https = app(&state, "203.0.113.7:40000", true);
    let (s, _, _) = send(&remote_https, rpc(Some(&bearer), "tools/list", json!({}))).await;
    assert_eq!(s, StatusCode::OK, "OAuth bearer over HTTPS");
    let (s, h, _) = send(&remote_https, rpc(None, "tools/list", json!({}))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(
        h[header::WWW_AUTHENTICATE],
        "Bearer resource_metadata=\"https://peck.example:3345/.well-known/oauth-protected-resource\""
    );
    // Agent tokens stay loopback-only, HTTPS or not.
    let agent = state.mcp_tokens.issue_token(session_id.clone(), None).await;
    let (s, _, _) = send(&remote_https, rpc(Some(&agent), "tools/list", json!({}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    let (s, _, meta) = send(
        &remote_https,
        Request::builder()
            .uri("/.well-known/oauth-authorization-server")
            .header(header::HOST, "peck.example:3345")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        meta["token_endpoint"],
        "https://peck.example:3345/oauth/token"
    );

    // ── Rotation kills the old secret and its tokens ──
    let (s, _, rotated) = send(
        &local,
        admin(
            Method::POST,
            &format!("/api/oauth-clients/{row_id}/rotate-secret"),
            &jwt,
            None,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let new_secret = rotated["client_secret"].as_str().unwrap().to_string();
    let (s, _, _) = send(&local, token_basic(&client_id, &secret)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "old secret still works");
    let (s, _, _) = send(&local, rpc(Some(&bearer), "tools/list", json!({}))).await;
    assert_eq!(
        s,
        StatusCode::UNAUTHORIZED,
        "pre-rotation token still works"
    );
    let (s, _, tok) = send(&local, token_basic(&client_id, &new_secret)).await;
    assert_eq!(s, StatusCode::OK);
    let bearer = tok["access_token"].as_str().unwrap().to_string();

    // ── Revocation ──
    let (s, _, _) = send(
        &local,
        admin(
            Method::DELETE,
            &format!("/api/oauth-clients/{row_id}"),
            &jwt,
            None,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _, _) = send(&local, rpc(Some(&bearer), "tools/list", json!({}))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _, _) = send(&local, token_basic(&client_id, &new_secret)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (_, _, list) = send(&local, admin(Method::GET, "/api/oauth-clients", &jwt, None)).await;
    assert!(list.as_array().unwrap().is_empty());
    assert!(state.db.get_session(&session_id).await.unwrap().is_some());
}
