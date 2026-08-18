//! A worker MCP token must not reach the admin/project tools.
//!
//! `worker_hidden_tool_names()` used to be trimmed from `tools/list` only,
//! and the per-handler checks (`ToolCallContext::scope_project`) enforce the
//! folder/project BOUNDARY, never the ROLE. A worker token is project-scoped,
//! so `delete_project` with no arguments resolved to the worker's OWN project
//! and cascaded it away, and `update_project` with a huge `worker_count`
//! raised the orchestrator's spawn cap — a money loop. `routes/mcp.rs` now
//! refuses those tools by name at dispatch for worker sessions, the same
//! shape as the pre-hatcher gate.
//!
//! This drives the real Axum `/mcp` router over loopback with per-session
//! bearer tokens, so it covers the gate where it actually runs.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use peckboard::auth::rate_limit::RateLimiter;
use peckboard::config::Config;
use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewProject, NewSession};
use peckboard::plugin::builtin::BuiltinPluginRegistry;
use peckboard::plugin::host::LiveHost;
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
        env_unlock: Arc::new(peckboard::service::env_vars::EnvUnlockRegistry::new()),
        config: Config {
            port: 0,
            https_port: 0,
            host: "127.0.0.1".into(),
            data_dir: data_dir.clone(),
            mdns: false,
            keep_alive_hours: 0,
            provider_send_timeout_secs: 300,
        },
        db,
        plugins,
        builtin_plugins: Arc::new(BuiltinPluginRegistry::new()),
        jwt_secret: vec![0u8; 32],
        ssh_vault_key: vec![0u8; 32],
        login_limiter: RateLimiter::new(100),
        password_change_limiter: RateLimiter::new(100),
        broadcaster: Broadcaster::new(),
        provider_registry: registry,
        session_manager,
        repeating_task_manager: peckboard::repeating::RepeatingTaskManager::new(),
        run_auditor: peckboard::repeating::RunAuditor::new(),
        mcp_tokens: McpTokenRegistry::new(),
        tls: Arc::new(peckboard::state::TlsState::new()),
        push_service: PushService::new(&data_dir),
    })
}

struct Fixture {
    state: Arc<AppState>,
    url: String,
    /// Token for a worker session bound to project `p1`.
    worker_token: String,
    /// Token for a plain (non-worker) chat session in the same folder.
    chat_token: String,
}

/// Seed a folder + project + a worker session and a chat session, then boot
/// the `/mcp` router on a loopback port.
async fn fixture() -> Fixture {
    let state = build_state().await;
    let ts = chrono::Utc::now().to_rfc3339();

    state
        .db
        .create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();

    state
        .db
        .create_project(NewProject {
            id: "p1".into(),
            name: "P".into(),
            context: "ctx".into(),
            folder_id: "f1".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "default".into(),
            model: None,
            effort: None,
            budget_usd_cents: None,
            budget_period: None,
            worktree_isolation: false,
            parallel_instructions: false,
            auto_notify_changes: true,
            worker_communication: false,
            created_at: ts.clone(),
            last_accessed_at: ts.clone(),
        })
        .await
        .unwrap();

    state
        .db
        .create_session(NewSession {
            id: "w1".into(),
            name: "worker".into(),
            folder_id: "f1".into(),
            is_worker: true,
            project_id: Some("p1".into()),
            created_at: ts.clone(),
            last_activity: ts.clone(),
            ..Default::default()
        })
        .await
        .unwrap();

    state
        .db
        .create_session(NewSession {
            id: "c1".into(),
            name: "chat".into(),
            folder_id: "f1".into(),
            is_worker: false,
            created_at: ts.clone(),
            last_activity: ts,
            ..Default::default()
        })
        .await
        .unwrap();

    let worker_token = state
        .mcp_tokens
        .issue_token("w1".into(), Some("p1".into()))
        .await;
    let chat_token = state.mcp_tokens.issue_token("c1".into(), None).await;

    let app = peckboard::routes::mcp::router(state.clone()).with_state(state.clone());
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

    Fixture {
        state,
        url: format!("http://{addr}/mcp"),
        worker_token,
        chat_token,
    }
}

async fn call_tool(
    url: &str,
    token: &str,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    let resp = reqwest::Client::new()
        .post(url)
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.json().await.unwrap()
}

#[tokio::test]
async fn worker_cannot_delete_its_own_project() {
    let fx = fixture().await;

    // No `project_id` argument at all — the path that used to resolve to the
    // worker's own project via the token scope and cascade it away.
    let body = call_tool(
        &fx.url,
        &fx.worker_token,
        "delete_project",
        serde_json::json!({}),
    )
    .await;

    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("delete_project") && msg.contains("blocked"),
        "expected a role refusal, got: {body}"
    );
    assert!(
        fx.state.db.get_project("p1").await.unwrap().is_some(),
        "project was deleted despite the gate"
    );
}

#[tokio::test]
async fn worker_cannot_raise_the_worker_count() {
    let fx = fixture().await;

    let body = call_tool(
        &fx.url,
        &fx.worker_token,
        "update_project",
        serde_json::json!({ "worker_count": 999 }),
    )
    .await;

    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("blocked"),
        "expected a role refusal, got: {body}"
    );
    assert_eq!(
        fx.state
            .db
            .get_project("p1")
            .await
            .unwrap()
            .unwrap()
            .worker_count,
        1,
        "worker_count was raised despite the gate"
    );
}

#[tokio::test]
async fn worker_tools_list_omits_the_admin_tools() {
    let fx = fixture().await;
    let resp = reqwest::Client::new()
        .post(&fx.url)
        .header("Authorization", format!("Bearer {}", fx.worker_token))
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
        }))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let names: Vec<&str> = body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(!names.contains(&"delete_project"), "{names:?}");
    assert!(names.contains(&"complete_step"), "{names:?}");
}

#[tokio::test]
async fn a_chat_session_can_still_update_the_project() {
    let fx = fixture().await;

    let body = call_tool(
        &fx.url,
        &fx.chat_token,
        "update_project",
        serde_json::json!({ "project_id": "p1", "worker_count": 3 }),
    )
    .await;

    assert!(body["error"].is_null(), "chat call was refused: {body}");
    assert_eq!(
        fx.state
            .db
            .get_project("p1")
            .await
            .unwrap()
            .unwrap()
            .worker_count,
        3
    );
}

#[tokio::test]
async fn worker_allowed_tools_still_work() {
    let fx = fixture().await;

    // A card tool the worker legitimately owns…
    let body = call_tool(
        &fx.url,
        &fx.worker_token,
        "list_cards",
        serde_json::json!({}),
    )
    .await;
    assert!(body["error"].is_null(), "list_cards was refused: {body}");

    // …and a common tool, to confirm the gate is name-scoped, not blanket.
    let body = call_tool(
        &fx.url,
        &fx.worker_token,
        "math",
        serde_json::json!({ "expression": "1 + 1" }),
    )
    .await;
    assert!(body["error"].is_null(), "math was refused: {body}");
}

struct NoopLiveHost;
impl LiveHost for NoopLiveHost {
    fn dispatch_capture(&self, _session_id: String, _prompt: String) {}
    fn resume_session(&self, _session_id: String, _text: String) {}
}

fn session_control_plugin_wasm() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "peck-plugins/session-control/target/wasm32-unknown-unknown/release/\
         peckboard_session_control_plugin.wasm",
    );
    p.exists().then_some(p)
}

/// Regression: `worker_hidden_tool_names()` only ever covered CORE tool
/// names, so a plugin-owned administrative tool (here session-control's
/// `clear_session`, which is folder-blind by design — see
/// `src/plugin/host.rs`) reached `invoke_mcp_tool` ungated for a worker
/// session. `ToolGate::with_plugin_tools` now denies it the same way core
/// admin tools are denied. Skips when the wasm isn't built locally.
#[tokio::test]
async fn worker_cannot_use_a_plugin_owned_administrative_tool() {
    let Some(wasm) = session_control_plugin_wasm() else {
        eprintln!(
            "SKIP worker_cannot_use_a_plugin_owned_administrative_tool: plugin wasm not built \
             (run peck-plugins/session-control/build.sh)"
        );
        return;
    };

    let fx = fixture().await;

    let plugins_dir = fx.state.config.data_dir.join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::copy(&wasm, plugins_dir.join("session-control.wasm")).unwrap();
    fx.state.plugins.load_all().await.unwrap();
    fx.state.plugins.set_live_host(Arc::new(NoopLiveHost));
    let info = fx
        .state
        .plugins
        .decide("session-control", true)
        .await
        .unwrap()
        .expect("session-control plugin should be loaded");
    assert_eq!(info.status, "approved", "plugin must be active: {info:?}");

    // Sanity: the tool is real and a chat session may still use it.
    let body = call_tool(
        &fx.url,
        &fx.chat_token,
        "clear_session",
        serde_json::json!({ "session_id": "w1" }),
    )
    .await;
    assert!(
        body["error"].is_null(),
        "chat call to a plugin admin tool was refused: {body}"
    );

    // The worker session must be refused, same as a core admin tool.
    let body = call_tool(
        &fx.url,
        &fx.worker_token,
        "clear_session",
        serde_json::json!({ "session_id": "c1" }),
    )
    .await;
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("clear_session") && msg.contains("blocked"),
        "expected a role refusal, got: {body}"
    );

    // …and it must not even be advertised.
    let resp = reqwest::Client::new()
        .post(&fx.url)
        .header("Authorization", format!("Bearer {}", fx.worker_token))
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
        }))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let names: Vec<&str> = body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(!names.contains(&"clear_session"), "{names:?}");
}
