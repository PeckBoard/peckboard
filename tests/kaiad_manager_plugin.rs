//! End-to-end test of the **kaiad-manager WASM plugin** against the real core
//! host functions, talking to a mock Kaiad MCP endpoint on loopback (which is
//! exactly what the `peckboard_http_request` host function exists to permit).
//!
//! Covers the full chain: `mcp.tool.invoke` dispatch → plugin → host fn →
//! stateless Streamable HTTP (`initialize` with **no** `Mcp-Session-Id`
//! issued, no `notifications/initialized`, sessionless `tools/list` /
//! `tools/call`) → SSE and JSON response framings → remote `isError`
//! surfacing → a deliberately slow (3 s) call to prove long host-function
//! work survives the plugin call timeout.
//!
//! The wasm is built out-of-tree (`peck-plugins/kaiad-manager/build.sh`) and
//! this repo's `cargo test` has no `wasm32` toolchain, so the test **skips**
//! with a note when the artifact is absent.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use peckboard::db::Db;
use peckboard::plugin::manager::PluginManager;
use serde_json::{Value, json};

const PLUGIN_ID: &str = "kaiad-manager";
const API_KEY: &str = "kop_test_token_0123456789";

fn plugin_wasm() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "../peck-plugins/kaiad-manager/target/wasm32-unknown-unknown/release/\
         peckboard_kaiad_manager_plugin.wasm",
    );
    p.exists().then_some(p)
}

/// What the mock Kaiad endpoint has seen / should enforce.
#[derive(Default)]
struct MockState {
    initialize_count: usize,
    /// A stateless server never issues a session id; a client echoing one
    /// anyway is a protocol bug this test must catch.
    saw_session_header: bool,
    /// `notifications/initialized` is meaningless without a session; the
    /// client must not send it.
    saw_initialized_notification: bool,
}

/// A minimal Streamable HTTP MCP server shaped like Kaiad's: Bearer-token
/// auth, **stateless** (fresh session per request, no `Mcp-Session-Id`
/// header ever issued), JSON *or* SSE response framing.
fn spawn_mock_kaiad(state: Arc<Mutex<MockState>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut sock) = stream else { continue };
            let state = state.clone();
            std::thread::spawn(move || {
                let Some((method, path, headers, body)) = read_http_request(&mut sock) else {
                    return;
                };
                let response = respond(&state, &method, &path, &headers, &body);
                let _ = sock.write_all(response.as_bytes());
            });
        }
    });
    port
}

/// Parse one HTTP/1.1 request: request line, headers (lowercased names), and
/// a Content-Length-delimited body.
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

fn http_response(status: &str, extra_headers: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n{extra_headers}\r\n{body}",
        body.len()
    )
}

fn json_response(status: &str, extra_headers: &str, v: &Value) -> String {
    http_response(
        status,
        &format!("content-type: application/json\r\n{extra_headers}"),
        &v.to_string(),
    )
}

/// The two tools the mock registers.
fn mock_tools() -> Value {
    json!([
        {
            "name": "list_services",
            "description": "List services in the tenant",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "trigger_build",
            "description": "Takes seconds, like a real build trigger",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": { "destructiveHint": false }
        }
    ])
}

fn respond(
    state: &Arc<Mutex<MockState>>,
    method: &str,
    path: &str,
    headers: &HashMap<String, String>,
    body: &str,
) -> String {
    if path != "/mcp" {
        return http_response("404 Not Found", "", "no such path");
    }
    if headers.get("authorization").map(String::as_str) != Some(&format!("Bearer {API_KEY}")[..]) {
        return json_response(
            "403 Forbidden",
            "",
            &json!({ "jsonrpc": "2.0", "error": { "code": -32001, "message": "credential lacks mcp scopes" }, "id": null }),
        );
    }
    // Stateless transport: a session header from the client is a bug.
    if headers.contains_key("mcp-session-id") {
        state.lock().unwrap().saw_session_header = true;
    }
    if method == "DELETE" {
        // Nothing to tear down statelessly; a polite 200 either way.
        return http_response("200 OK", "", "");
    }
    let msg: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let rpc_method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = msg.get("id").cloned().unwrap_or(Value::Null);

    match rpc_method {
        "initialize" => {
            let mut st = state.lock().unwrap();
            st.initialize_count += 1;
            let result = json!({
                "protocolVersion": "2025-03-26",
                "serverInfo": { "name": "kaiad-mcp", "version": "1.0.0" },
                "capabilities": { "tools": {} },
                "instructions": "Tools to manage a Kaiad control plane.",
            });
            // Deliberately NO mcp-session-id header: stateless mode.
            json_response(
                "200 OK",
                "",
                &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            )
        }
        "notifications/initialized" => {
            state.lock().unwrap().saw_initialized_notification = true;
            http_response("202 Accepted", "", "")
        }
        // SSE-framed on purpose: the SDK often answers this way.
        "tools/list" => {
            let msg = json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": mock_tools() } });
            let body = format!("event: message\ndata: {msg}\n\n");
            http_response("200 OK", "content-type: text/event-stream\r\n", &body)
        }
        "tools/call" => {
            let name = msg
                .pointer("/params/name")
                .and_then(|n| n.as_str())
                .unwrap_or("");
            match name {
                "list_services" => {
                    let text = "[{\"id\":\"svc-1\",\"name\":\"api\",\"kind\":\"web\"}]";
                    json_response(
                        "200 OK",
                        "",
                        &json!({ "jsonrpc": "2.0", "id": id, "result": {
                            "content": [{ "type": "text", "text": text }], "isError": false
                        } }),
                    )
                }
                "trigger_build" => {
                    // Longer than the 2 s Extism plugin-call timeout: proves
                    // time spent inside a host function doesn't kill the call.
                    std::thread::sleep(std::time::Duration::from_secs(3));
                    json_response(
                        "200 OK",
                        "",
                        &json!({ "jsonrpc": "2.0", "id": id, "result": {
                            "content": [{ "type": "text", "text": "{\"build\":\"queued\"}" }], "isError": false
                        } }),
                    )
                }
                other => json_response(
                    "200 OK",
                    "",
                    &json!({ "jsonrpc": "2.0", "id": id, "result": {
                        "content": [{ "type": "text", "text": format!("Error: tool {other} not found") }],
                        "isError": true
                    } }),
                ),
            }
        }
        _ => json_response(
            "400 Bad Request",
            "",
            &json!({ "jsonrpc": "2.0", "error": { "code": -32600, "message": "unsupported method" }, "id": id }),
        ),
    }
}

async fn invoke(plugins: &PluginManager, tool: &str, args: Value, ctx: &Value) -> Value {
    plugins
        .invoke_mcp_tool(tool, args, ctx.clone())
        .await
        .expect("plugin should own this tool")
        .unwrap_or_else(|e| panic!("{tool} failed: {e}"))
}

#[tokio::test]
async fn kaiad_manager_plugin_bridges_a_mock_kaiad_end_to_end() {
    let Some(wasm) = plugin_wasm() else {
        eprintln!(
            "SKIP kaiad_manager_plugin_bridges_a_mock_kaiad_end_to_end: plugin wasm not built \
             (run peck-plugins/kaiad-manager/build.sh)"
        );
        return;
    };

    let state = Arc::new(Mutex::new(MockState::default()));
    let port = spawn_mock_kaiad(state.clone());
    let base_url = format!("http://127.0.0.1:{port}");

    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path();
    let plugins_dir = data_dir.join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::copy(&wasm, plugins_dir.join(format!("{PLUGIN_ID}.wasm"))).unwrap();

    let db = Db::open(data_dir).unwrap();
    let plugins = PluginManager::new(data_dir, db.clone());
    plugins.load_all().await.unwrap();
    let info = plugins
        .decide(PLUGIN_ID, true)
        .await
        .unwrap()
        .expect("kaiad-manager plugin should be loaded");
    assert_eq!(info.status, "approved", "plugin must be active: {info:?}");

    // The manifest-declared settings surface: URL + secret token, resolved by
    // id — the same lookup the /api/plugins settings routes use.
    assert_eq!(info.settings_schema.fields.len(), 2);
    let schema = plugins
        .settings_schema_for(PLUGIN_ID)
        .await
        .expect("loaded plugin must expose its settings schema");
    assert_eq!(schema.fields[0].key, "base_url");
    assert_eq!(schema.fields[1].key, "api_key");

    let ctx = json!({ "sessionId": "chat-1" });

    // Unconfigured status is a diagnosis, not an error.
    let res = invoke(&plugins, "kaiad_status", json!({}), &ctx).await;
    assert_eq!(res["configured"], json!(false), "status: {res}");

    // Configure + verify: stateless handshake against the mock.
    let res = invoke(
        &plugins,
        "kaiad_configure",
        json!({ "base_url": base_url, "api_key": API_KEY }),
        &ctx,
    )
    .await;
    assert_eq!(res["saved"], json!(true), "configure: {res}");
    assert_eq!(res["verified"], json!(true), "configure: {res}");
    assert_eq!(res["server"]["name"], json!("kaiad-mcp"));
    assert_eq!(res["tool_count"], json!(2));
    assert!(
        !res["api_key"].as_str().unwrap().contains("0123456789"),
        "token must be masked: {res}"
    );

    // Status now handshakes and reports the catalog.
    let res = invoke(&plugins, "kaiad_status", json!({}), &ctx).await;
    assert_eq!(res["connected"], json!(true), "status: {res}");
    assert_eq!(res["tools"][0], json!("list_services"));

    // Compact catalog (SSE-framed tools/list on the wire) + full schema fetch.
    let res = invoke(&plugins, "kaiad_list_tools", json!({}), &ctx).await;
    assert_eq!(res["count"], json!(2), "list: {res}");
    assert_eq!(res["tools"][0]["read_only"], json!(true));
    let res = invoke(
        &plugins,
        "kaiad_list_tools",
        json!({ "names": ["list_services", "nonexistent_tool"] }),
        &ctx,
    )
    .await;
    assert_eq!(res["tools"][0]["inputSchema"]["type"], json!("object"));
    assert_eq!(res["missing"]["names"][0], json!("nonexistent_tool"));

    // Proxy a call; JSON text content comes back as JSON.
    let res = invoke(
        &plugins,
        "kaiad_call",
        json!({ "name": "list_services" }),
        &ctx,
    )
    .await;
    assert_eq!(res[0]["id"], json!("svc-1"), "call: {res}");
    assert_eq!(res[0]["name"], json!("api"));

    // isError from the remote surfaces as a tool error, not a payload.
    let err = plugins
        .invoke_mcp_tool("kaiad_call", json!({ "name": "bogus_tool" }), ctx.clone())
        .await
        .expect("owned")
        .expect_err("remote isError must become a tool error");
    assert!(err.to_string().contains("not found"), "{err}");

    // A 3 s remote operation (build-trigger-shaped) survives the 2 s Extism
    // plugin-call timeout because the wait happens host-side.
    let res = invoke(
        &plugins,
        "kaiad_call",
        json!({ "name": "trigger_build" }),
        &ctx,
    )
    .await;
    assert_eq!(res["build"], json!("queued"), "slow call: {res}");

    // Stateless discipline: the server never issued a session id, so the
    // client must never have echoed one, nor sent notifications/initialized.
    let st = state.lock().unwrap();
    assert!(
        !st.saw_session_header,
        "client sent Mcp-Session-Id against a stateless server"
    );
    assert!(
        !st.saw_initialized_notification,
        "client sent notifications/initialized without a session"
    );
    assert!(st.initialize_count >= 1, "client never initialized");
}
