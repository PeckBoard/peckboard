//! Integration test for the **notifier WASM plugin** against real core host
//! functions. Fires the five lifecycle notification hooks via
//! `PluginManager::dispatch_notify`, asserts that the plugin POSTs to a mock
//! HTTP server running on loopback, and verifies the filter settings
//! (`events` list, `workers_only`).
//!
//! The wasm is built out-of-tree (`peck-plugins/notifier/build.sh`) and this
//! repo's `cargo test` has no js-pdk toolchain, so the test **skips** with a
//! note when the artifact is absent.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use peckboard::db::Db;
use peckboard::plugin::manager::PluginManager;
use serde_json::json;

const PLUGIN_ID: &str = "notifier";

fn plugin_wasm() -> Option<PathBuf> {
    let p =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../peck-plugins/notifier/dist/plugin.wasm");
    p.exists().then_some(p)
}

// ── Minimal mock HTTP server ─────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
struct CapturedRequest {
    path: String,
    headers: HashMap<String, String>,
    body: String,
}

#[derive(Default)]
struct MockState {
    requests: Vec<CapturedRequest>,
}

fn spawn_mock_server(state: Arc<Mutex<MockState>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut sock) = stream else { continue };
            let state = state.clone();
            std::thread::spawn(move || {
                let Some((_, path, headers, body)) = read_http_request(&mut sock) else {
                    return;
                };
                {
                    let mut st = state.lock().unwrap();
                    st.requests.push(CapturedRequest {
                        path,
                        headers,
                        body,
                    });
                }
                let response = "HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                let _ = sock.write_all(response.as_bytes());
            });
        }
    });
    port
}

fn read_http_request(
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

// ── Helpers ──────────────────────────────────────────────────────────────────

async fn setup(wasm: &PathBuf) -> (Db, PluginManager, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path();
    let plugins_dir = data_dir.join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::copy(wasm, plugins_dir.join(format!("{PLUGIN_ID}.wasm"))).unwrap();

    let db = Db::open(data_dir).unwrap();
    let plugins = PluginManager::new(data_dir, db.clone());
    plugins.load_all().await.unwrap();
    let info = plugins
        .decide(PLUGIN_ID, true)
        .await
        .unwrap()
        .expect("notifier plugin should be loaded");
    assert_eq!(info.status, "approved", "plugin must be active: {info:?}");

    (db, plugins, dir)
}

fn take_requests(state: &Arc<Mutex<MockState>>) -> Vec<CapturedRequest> {
    let mut st = state.lock().unwrap();
    std::mem::take(&mut st.requests)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn notifier_plugin_posts_all_five_hooks() {
    let Some(wasm) = plugin_wasm() else {
        eprintln!(
            "SKIP notifier_plugin_posts_all_five_hooks: plugin wasm not built \
             (run peck-plugins/notifier/build.sh)"
        );
        return;
    };

    let state = Arc::new(Mutex::new(MockState::default()));
    let port = spawn_mock_server(state.clone());
    let base_url = format!("http://127.0.0.1:{port}");

    let (db, plugins, _dir) = setup(&wasm).await;

    // Configure ntfy + generic webhook; leave Telegram/Discord unconfigured.
    db.set_plugin_setting(PLUGIN_ID, "ntfy_url", &json!(base_url.clone()))
        .await
        .unwrap();
    db.set_plugin_setting(PLUGIN_ID, "ntfy_topic", &json!("peck"))
        .await
        .unwrap();
    db.set_plugin_setting(
        PLUGIN_ID,
        "generic_webhook_url",
        &json!(format!("{base_url}/generic")),
    )
    .await
    .unwrap();

    // Fire all five hooks.
    plugins.dispatch_notify(
        "card.step.after",
        json!({
            "card_id": "c1", "card_title": "Finish feature",
            "project_id": "p1", "project_name": "My App",
            "old_step": "in_progress", "new_step": "done", "terminal": true
        }),
    );
    plugins.dispatch_notify(
        "session.agent.ended",
        json!({
            "session_id": "s1", "session_name": "Worker",
            "is_worker": true, "outcome": "completed", "reason": null
        }),
    );
    plugins.dispatch_notify(
        "worker.blocked",
        json!({
            "card_id": "c1", "card_title": "Finish feature",
            "project_id": "p1", "project_name": "My App",
            "reason": "too many crashes"
        }),
    );
    plugins.dispatch_notify(
        "project.paused",
        json!({
            "project_id": "p1", "project_name": "My App",
            "reason": "too many crashes", "source": "crash"
        }),
    );
    plugins.dispatch_notify(
        "question.pending",
        json!({
            "session_id": "s1", "session_name": "Worker",
            "preview": "Should I proceed?"
        }),
    );

    // Give the spawned tasks time to complete.
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;

    let reqs = take_requests(&state);

    // 5 ntfy POSTs to /peck, 5 generic POSTs to /generic.
    let ntfy_reqs: Vec<_> = reqs.iter().filter(|r| r.path == "/peck").collect();
    let generic_reqs: Vec<_> = reqs.iter().filter(|r| r.path == "/generic").collect();

    assert_eq!(
        ntfy_reqs.len(),
        5,
        "expected 5 ntfy POSTs, got {}: {reqs:?}",
        ntfy_reqs.len()
    );
    assert_eq!(
        generic_reqs.len(),
        5,
        "expected 5 generic POSTs, got {}: {reqs:?}",
        generic_reqs.len()
    );

    // ntfy requests carry a Title header.
    for r in &ntfy_reqs {
        assert!(
            r.headers.contains_key("title"),
            "ntfy request missing Title header: {r:?}"
        );
        assert!(
            !r.headers["title"].is_empty(),
            "ntfy Title header is empty: {r:?}"
        );
    }

    // Verify one specific ntfy message for card.step.after.
    let card_ntfy = ntfy_reqs
        .iter()
        .find(|r| {
            r.headers
                .get("title")
                .map(|t| t.contains("Card done"))
                .unwrap_or(false)
        })
        .expect("expected a ntfy request with 'Card done' in Title");
    assert!(
        card_ntfy.body.contains("My App"),
        "ntfy body should contain project: {card_ntfy:?}"
    );

    // Generic webhook payloads carry the `hook` field.
    let card_generic = generic_reqs
        .iter()
        .find(|r| r.body.contains("card.step.after"))
        .expect("expected generic POST for card.step.after");
    let val: serde_json::Value = serde_json::from_str(&card_generic.body).unwrap();
    assert_eq!(val["hook"], json!("card.step.after"));
    assert_eq!(val["title"], json!("Card done: Finish feature"));
}

#[tokio::test]
async fn notifier_events_filter_drops_non_listed_hooks() {
    let Some(wasm) = plugin_wasm() else {
        eprintln!("SKIP notifier_events_filter_drops_non_listed_hooks: wasm not built");
        return;
    };

    let state = Arc::new(Mutex::new(MockState::default()));
    let port = spawn_mock_server(state.clone());
    let base_url = format!("http://127.0.0.1:{port}");

    let (db, plugins, _dir) = setup(&wasm).await;

    db.set_plugin_setting(PLUGIN_ID, "ntfy_url", &json!(base_url))
        .await
        .unwrap();
    db.set_plugin_setting(PLUGIN_ID, "ntfy_topic", &json!("peck"))
        .await
        .unwrap();
    // Only forward card.step.after.
    db.set_plugin_setting(PLUGIN_ID, "events", &json!(["card.step.after"]))
        .await
        .unwrap();

    plugins.dispatch_notify(
        "card.step.after",
        json!({
            "card_id": "c1", "card_title": "T", "project_id": "p1",
            "project_name": "P", "old_step": "backlog", "new_step": "done", "terminal": true
        }),
    );
    plugins.dispatch_notify(
        "session.agent.ended",
        json!({
            "session_id": "s1", "session_name": "S",
            "is_worker": true, "outcome": "completed", "reason": null
        }),
    );
    plugins.dispatch_notify(
        "question.pending",
        json!({"session_id": "s1", "session_name": "S", "preview": "Q?"}),
    );

    tokio::time::sleep(std::time::Duration::from_millis(800)).await;

    let reqs = take_requests(&state);
    let ntfy_reqs: Vec<_> = reqs.iter().filter(|r| r.path == "/peck").collect();
    assert_eq!(
        ntfy_reqs.len(),
        1,
        "only card.step.after should pass events filter, got {}: {reqs:?}",
        ntfy_reqs.len()
    );
}

#[tokio::test]
async fn notifier_workers_only_drops_non_worker_session() {
    let Some(wasm) = plugin_wasm() else {
        eprintln!("SKIP notifier_workers_only_drops_non_worker_session: wasm not built");
        return;
    };

    let state = Arc::new(Mutex::new(MockState::default()));
    let port = spawn_mock_server(state.clone());
    let base_url = format!("http://127.0.0.1:{port}");

    let (db, plugins, _dir) = setup(&wasm).await;

    db.set_plugin_setting(PLUGIN_ID, "ntfy_url", &json!(base_url))
        .await
        .unwrap();
    db.set_plugin_setting(PLUGIN_ID, "ntfy_topic", &json!("peck"))
        .await
        .unwrap();
    db.set_plugin_setting(PLUGIN_ID, "workers_only", &json!(true))
        .await
        .unwrap();

    // Non-worker session — should be dropped.
    plugins.dispatch_notify(
        "session.agent.ended",
        json!({
            "session_id": "s1", "session_name": "Chat",
            "is_worker": false, "outcome": "completed", "reason": null
        }),
    );
    // Worker session — should pass.
    plugins.dispatch_notify(
        "session.agent.ended",
        json!({
            "session_id": "s2", "session_name": "Worker",
            "is_worker": true, "outcome": "crashed", "reason": "OOM"
        }),
    );

    tokio::time::sleep(std::time::Duration::from_millis(800)).await;

    let reqs = take_requests(&state);
    let ntfy_reqs: Vec<_> = reqs.iter().filter(|r| r.path == "/peck").collect();
    assert_eq!(
        ntfy_reqs.len(),
        1,
        "only the worker session should fire, got {}: {reqs:?}",
        ntfy_reqs.len()
    );
    assert!(
        ntfy_reqs[0]
            .headers
            .get("title")
            .map(|t| t.contains("crashed"))
            .unwrap_or(false),
        "the surviving notification should be for the crashed worker: {:?}",
        ntfy_reqs[0]
    );
}
