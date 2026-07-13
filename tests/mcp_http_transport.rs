//! End-to-end test for the MCP server's native HTTP transport.
//!
//! Agents connect to the in-process Rust MCP server (`/mcp`) directly over
//! HTTP — there is no longer a Node stdio-to-HTTP proxy bridging the CLI. For
//! that to work, the route itself must answer the full MCP lifecycle the proxy
//! used to fake locally: `initialize`, the `notifications/initialized`
//! notification, plus the `tools/list` / `tools/call` it always forwarded.
//!
//! This boots the real Axum `/mcp` router over a loopback TCP socket and drives
//! the whole handshake with an HTTP client carrying the per-session bearer
//! token in the `Authorization` header — exactly as the CLI's HTTP MCP client
//! does. If any lifecycle method regressed, the proxy-free path would break and
//! this test catches it.

use std::net::SocketAddr;
use std::sync::Arc;

use peckboard::auth::rate_limit::RateLimiter;
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewSession};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::manager::PluginManager;
use peckboard::provider::manager::SessionManager;
use peckboard::provider::registry::ProviderRegistry;
use peckboard::service::mcp_server::McpTokenRegistry;
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

/// Boot the `/mcp` router on a loopback port and return its base URL plus a
/// bearer token bound to a freshly-seeded session.
async fn serve_mcp() -> (String, String) {
    let state = build_state().await;
    let ts = chrono::Utc::now().to_rfc3339();

    state
        .db
        .create_folder(NewFolder {
            id: "f1".into(),
            name: "Folder".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
    state
        .db
        .create_session(NewSession {
            id: "s1".into(),
            name: "worker".into(),
            folder_id: "f1".into(),
            is_worker: true,
            created_at: ts.clone(),
            last_activity: ts,
            ..Default::default()
        })
        .await
        .unwrap();

    let token = state.mcp_tokens.issue_token("s1".into(), None).await;

    let app = peckboard::routes::mcp::router(state.clone()).with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    (format!("http://{addr}/mcp"), token)
}

#[tokio::test]
async fn http_transport_serves_full_mcp_lifecycle_without_a_proxy() {
    let (url, token) = serve_mcp().await;
    let client = reqwest::Client::new();
    let bearer = format!("Bearer {token}");

    // 1. initialize — the route (not a node proxy) answers the handshake.
    let resp = client
        .post(&url)
        .header("Authorization", &bearer)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2025-03-26" },
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["result"]["serverInfo"]["name"], "peckboard");
    // Client's requested protocol version is echoed back.
    assert_eq!(body["result"]["protocolVersion"], "2025-03-26");
    assert!(body["result"]["capabilities"]["tools"].is_object());

    // 2. notifications/initialized — no `id`, so no JSON-RPC body, just 202.
    let resp = client
        .post(&url)
        .header("Authorization", &bearer)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    // 3. tools/list — the core tool set comes back over HTTP.
    let resp = client
        .post(&url)
        .header("Authorization", &bearer)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let names: Vec<&str> = body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"create_card"),
        "missing create_card: {names:?}"
    );

    // 4. tools/call — a real tool round-trips end to end over HTTP.
    let resp = client
        .post(&url)
        .header("Authorization", &bearer)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "list_workflows", "arguments": {} },
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["result"]["content"].is_array(), "got: {body}");
}

#[tokio::test]
async fn http_transport_rejects_missing_bearer() {
    let (url, _token) = serve_mcp().await;
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}
