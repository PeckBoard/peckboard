//! Integration test for the **github-bridge WASM plugin** against the real core
//! host functions, talking to a mock GitHub REST API on loopback.
//!
//! Covers the full chain:
//!   - `gh_status` (unconfigured → configured with rate-limit)
//!   - `gh_import_issues` → `peckboard_create_card` + `peckboard_store_put`
//!   - `gh_link_pr` → store update + comment POST
//!   - `gh_sync_card` → comment POST + no-close on non-terminal step
//!   - `card.step.after` hook (auto-close via `dispatch_notify`, idempotent)
//!
//! The wasm is built out-of-tree (`peck-plugins/github-bridge/build.sh`) and
//! this repo's `cargo test` has no js-pdk toolchain, so the test **skips**
//! with a note when the artifact is absent.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewProject, NewSession};
use peckboard::plugin::manager::PluginManager;
use serde_json::{Value, json};

const PLUGIN_ID: &str = "github-bridge";
const GITHUB_TOKEN: &str = "ghp_test_token_abcdef";
const REPO: &str = "test-owner/test-repo";

fn plugin_wasm() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../peck-plugins/github-bridge/dist/plugin.wasm");
    p.exists().then_some(p)
}

// ── Mock GitHub HTTP server ──────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
struct CapturedRequest {
    method: String,
    path: String,
    body: String,
}

#[derive(Default)]
struct MockState {
    requests: Vec<CapturedRequest>,
}

fn spawn_mock_github(state: Arc<Mutex<MockState>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut sock) = stream else { continue };
            let state = state.clone();
            std::thread::spawn(move || {
                let Some((method, path, _headers, body)) = read_http(&mut sock) else {
                    return;
                };
                let response = respond_github(&method, &path);
                {
                    let mut st = state.lock().unwrap();
                    st.requests.push(CapturedRequest { method, path, body });
                }
                let _ = sock.write_all(response.as_bytes());
            });
        }
    });
    port
}

fn read_http(
    sock: &mut std::net::TcpStream,
) -> Option<(String, String, HashMap<String, String>, String)> {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    let header_end = loop {
        let n = sock.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&buf[..n]);
        if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        if data.len() > 1 << 20 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&data[..header_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    while data.len() < header_end + content_length {
        let n = sock.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    let body = String::from_utf8_lossy(&data[header_end..]).into_owned();
    Some((method, path, headers, body))
}

fn json_resp(status: &str, body: &Value) -> String {
    let s = body.to_string();
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{s}",
        s.len()
    )
}

fn respond_github(method: &str, path: &str) -> String {
    let bare = path.split('?').next().unwrap_or(path);

    if method == "GET" && bare == "/rate_limit" {
        return json_resp(
            "200 OK",
            &json!({ "rate": { "limit": 5000, "remaining": 4900, "reset": 9999999999_u64 } }),
        );
    }

    let issues_path = format!("/repos/{REPO}/issues");
    if method == "GET" && bare == issues_path {
        return json_resp(
            "200 OK",
            &json!([
                {
                    "number": 1,
                    "title": "Fix the bug",
                    "body": "Something is very wrong.",
                    "state": "open",
                    "html_url": "https://github.com/test-owner/test-repo/issues/1"
                },
                {
                    "number": 2,
                    "title": "Add a feature",
                    "body": "We need this feature.",
                    "state": "open",
                    "html_url": "https://github.com/test-owner/test-repo/issues/2"
                }
            ]),
        );
    }

    if method == "POST"
        && bare.starts_with(&format!("/repos/{REPO}/issues/"))
        && bare.ends_with("/comments")
    {
        return json_resp("201 Created", &json!({ "id": 1, "body": "ok" }));
    }

    if method == "PATCH" && bare.starts_with(&format!("/repos/{REPO}/issues/")) {
        return json_resp("200 OK", &json!({ "number": 1, "state": "closed" }));
    }

    format!("HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
}

// ── Helpers ───────────────────────────────────────────────────────────────────

async fn invoke(plugins: &PluginManager, tool: &str, args: Value, ctx: &Value) -> Value {
    plugins
        .invoke_mcp_tool(tool, args, ctx.clone())
        .await
        .expect("plugin should own this tool")
        .unwrap_or_else(|e| panic!("{tool} failed: {e}"))
}

fn take_requests(state: &Arc<Mutex<MockState>>) -> Vec<CapturedRequest> {
    std::mem::take(&mut state.lock().unwrap().requests)
}

// ── Test ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn github_bridge_plugin_drives_tools_end_to_end() {
    let Some(wasm) = plugin_wasm() else {
        eprintln!(
            "SKIP github_bridge_plugin_drives_tools_end_to_end: plugin wasm not built \
             (run peck-plugins/github-bridge/build.sh)"
        );
        return;
    };

    let state = Arc::new(Mutex::new(MockState::default()));
    let port = spawn_mock_github(state.clone());
    let api_base = format!("http://127.0.0.1:{port}");

    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path();
    let plugins_dir = data_dir.join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::copy(&wasm, plugins_dir.join(format!("{PLUGIN_ID}.wasm"))).unwrap();

    let db = Db::open(data_dir).unwrap();
    let ts = chrono::Utc::now().to_rfc3339();

    db.create_folder(NewFolder {
        id: "f1".into(),
        name: "Test folder".into(),
        path: data_dir.to_string_lossy().to_string(),
        created_at: ts.clone(),
    })
    .await
    .unwrap();
    db.create_project(NewProject {
        id: "proj-1".into(),
        name: "Test project".into(),
        context: String::new(),
        folder_id: "f1".into(),
        worker_count: 1,
        status: "active".into(),
        workflow: "fast-develop-software".into(),
        model: None,
        effort: None,
        budget_usd_cents: None,
        budget_period: None,
        parallel_instructions: false,
        auto_notify_changes: false,
        worker_communication: false,
        created_at: ts.clone(),
        last_accessed_at: ts.clone(),
    })
    .await
    .unwrap();
    db.create_session(NewSession {
        id: "caller-1".into(),
        name: "Caller".into(),
        folder_id: "f1".into(),
        project_id: Some("proj-1".into()),
        is_worker: true,
        created_at: ts.clone(),
        last_activity: ts,
        ..Default::default()
    })
    .await
    .unwrap();

    let plugins = PluginManager::new(data_dir, db.clone());
    plugins.load_all().await.unwrap();
    let info = plugins
        .decide(PLUGIN_ID, true)
        .await
        .unwrap()
        .expect("github-bridge plugin should be loaded");
    assert_eq!(info.status, "approved", "plugin must be active: {info:?}");

    let ctx = json!({ "sessionId": "caller-1", "projectId": "proj-1", "folderId": "f1" });

    // ── gh_status: unconfigured ───────────────────────────────────────────────
    let res = invoke(&plugins, "gh_status", json!({}), &ctx).await;
    assert_eq!(res["configured"], json!(false), "unconfigured: {res}");
    let missing = res["missing"].as_array().expect("missing array");
    assert!(
        missing.iter().any(|m| m.as_str() == Some("github_token")),
        "should list github_token missing: {res}"
    );

    // ── configure settings ────────────────────────────────────────────────────
    db.set_plugin_setting(PLUGIN_ID, "github_token", &json!(GITHUB_TOKEN))
        .await
        .unwrap();
    db.set_plugin_setting(PLUGIN_ID, "repo", &json!(REPO))
        .await
        .unwrap();
    db.set_plugin_setting(PLUGIN_ID, "project_id", &json!("proj-1"))
        .await
        .unwrap();
    db.set_plugin_setting(PLUGIN_ID, "github_api_base", &json!(api_base))
        .await
        .unwrap();
    db.set_plugin_setting(PLUGIN_ID, "import_step", &json!("backlog"))
        .await
        .unwrap();

    // ── gh_status: configured + rate-limit ───────────────────────────────────
    take_requests(&state);
    let res = invoke(&plugins, "gh_status", json!({}), &ctx).await;
    assert_eq!(res["configured"], json!(true), "configured status: {res}");
    assert_eq!(res["repo"], json!(REPO));
    assert_eq!(res["mapping_count"], json!(0));
    assert_eq!(res["rate_limit"]["remaining"], json!(4900));

    let reqs = take_requests(&state);
    assert!(
        reqs.iter()
            .any(|r| r.method == "GET" && r.path.contains("rate_limit")),
        "should hit /rate_limit: {reqs:?}"
    );

    // ── gh_import_issues: two issues → two cards ──────────────────────────────
    let res = invoke(&plugins, "gh_import_issues", json!({}), &ctx).await;
    assert_eq!(res["imported"], json!(2), "import: {res}");
    assert_eq!(res["skipped"], json!(0), "import: {res}");

    let reqs = take_requests(&state);
    assert!(
        reqs.iter()
            .any(|r| r.method == "GET" && r.path.contains("/issues")),
        "should fetch issues: {reqs:?}"
    );

    // Verify cards were created
    let cards = db
        .list_cards_by_project("proj-1")
        .await
        .expect("list cards");
    assert_eq!(cards.len(), 2, "should have 2 cards: {cards:?}");
    let card1 = cards
        .iter()
        .find(|c| c.title.contains("#1"))
        .expect("card for issue #1");
    assert_eq!(card1.step, "backlog");
    assert!(
        card1.description.contains("https://github.com"),
        "description should include issue URL: {}",
        card1.description
    );

    // gh_status now shows 2 mappings
    take_requests(&state);
    let res = invoke(&plugins, "gh_status", json!({}), &ctx).await;
    assert_eq!(res["mapping_count"], json!(2), "after import: {res}");

    // ── re-import skips already-mapped issues ─────────────────────────────────
    take_requests(&state);
    let res = invoke(&plugins, "gh_import_issues", json!({}), &ctx).await;
    assert_eq!(
        res["imported"],
        json!(0),
        "re-import should import 0: {res}"
    );
    assert_eq!(res["skipped"], json!(2), "re-import should skip 2: {res}");

    // ── gh_link_pr ────────────────────────────────────────────────────────────
    take_requests(&state);
    let card1_id = card1.id.clone();
    let res = invoke(
        &plugins,
        "gh_link_pr",
        json!({ "card_id": card1_id, "pr_number": 99 }),
        &ctx,
    )
    .await;
    assert_eq!(res["ok"], json!(true), "link_pr: {res}");
    assert_eq!(res["pr_number"], json!(99));

    let reqs = take_requests(&state);
    assert!(
        reqs.iter()
            .any(|r| r.method == "POST" && r.path.contains("/comments")),
        "should post PR link comment: {reqs:?}"
    );

    // ── gh_sync_card (non-terminal — backlog) ─────────────────────────────────
    take_requests(&state);
    let res = invoke(
        &plugins,
        "gh_sync_card",
        json!({ "card_id": card1_id }),
        &ctx,
    )
    .await;
    assert_eq!(res["ok"], json!(true), "sync_card backlog: {res}");
    assert_eq!(res["issue_number"], json!(1));

    let reqs = take_requests(&state);
    assert!(
        reqs.iter()
            .any(|r| r.method == "POST" && r.path.contains("/comments")),
        "sync should post step comment: {reqs:?}"
    );
    assert!(
        !reqs.iter().any(|r| r.method == "PATCH"),
        "sync should not close issue on non-terminal step: {reqs:?}"
    );

    // ── gh_sync_card: unknown card returns error ──────────────────────────────
    let res = invoke(
        &plugins,
        "gh_sync_card",
        json!({ "card_id": "no-such-card" }),
        &ctx,
    )
    .await;
    assert!(
        res["error"].is_string(),
        "unknown card should return error: {res}"
    );

    // ── card.step.after hook: auto-close on done ──────────────────────────────
    take_requests(&state);
    let card1_title = card1.title.clone();
    plugins.dispatch_notify(
        "card.step.after",
        json!({
            "card_id": card1_id,
            "card_title": card1_title,
            "project_id": "proj-1",
            "project_name": "Test project",
            "old_step": "backlog",
            "new_step": "done",
            "terminal": true
        }),
    );
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;

    let reqs = take_requests(&state);
    assert!(
        reqs.iter()
            .any(|r| r.method == "POST" && r.path.contains("/comments")),
        "hook should post completion comment: {reqs:?}"
    );
    assert!(
        reqs.iter()
            .any(|r| r.method == "PATCH" && r.path.contains("/issues/1")),
        "hook should close issue: {reqs:?}"
    );

    // ── auto-close is idempotent: second terminal event is a no-op ────────────
    take_requests(&state);
    plugins.dispatch_notify(
        "card.step.after",
        json!({
            "card_id": card1_id,
            "card_title": card1_title,
            "project_id": "proj-1",
            "project_name": "Test project",
            "old_step": "backlog",
            "new_step": "done",
            "terminal": true
        }),
    );
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let reqs = take_requests(&state);
    assert!(
        reqs.is_empty(),
        "second terminal on closed issue must be a no-op: {reqs:?}"
    );

    // ── non-terminal step event is ignored ────────────────────────────────────
    take_requests(&state);
    plugins.dispatch_notify(
        "card.step.after",
        json!({
            "card_id": card1_id,
            "card_title": card1_title,
            "project_id": "proj-1",
            "project_name": "Test project",
            "old_step": "backlog",
            "new_step": "in_progress",
            "terminal": false
        }),
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let reqs = take_requests(&state);
    assert!(
        reqs.is_empty(),
        "non-terminal step should make no GitHub calls: {reqs:?}"
    );
}
