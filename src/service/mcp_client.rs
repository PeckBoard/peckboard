//! Minimal MCP *client* for user-defined servers (Settings → MCP Servers).
//!
//! The Ollama provider runs its own tool loop in-process, so external MCP
//! servers are reached natively from Rust — no bridge binary (the same
//! philosophy that removed the Node stdio proxy from the server side; see
//! `service::mcp_server::config`). Deliberately small rather than a full
//! SDK:
//!
//! - **stdio**: spawn `command args…` with the configured env; JSON-RPC
//!   messages are newline-delimited on stdin/stdout.
//! - **streamable HTTP** (`http`/`sse` entries): POST JSON-RPC to the URL
//!   with the configured headers; both `application/json` and single-shot
//!   `text/event-stream` responses are handled, and an `Mcp-Session-Id`
//!   issued at `initialize` is echoed on later requests. (Classic
//!   two-endpoint SSE servers are not supported; modern servers labelled
//!   "sse" in the editor usually accept streamable POSTs.)
//!
//! Requests are lock-step — one in flight per server — which the tool loop
//! guarantees. Server-initiated requests that arrive between responses get
//! a best-effort reply (`ping` → `{}`, anything else → "not supported") so
//! a chatty server can't stall the turn. Every wire wait is bounded by a
//! timeout, and tool output is truncated at [`MAX_RESULT_CHARS`] so a
//! runaway server can't blow up the model context.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

/// Bound on connect + `initialize` and on `tools/list`.
const SETUP_TIMEOUT: Duration = Duration::from_secs(15);
/// Bound on one `tools/call`.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Tool output beyond this many characters is truncated with a marker.
const MAX_RESULT_CHARS: usize = 48_000;
/// Header carrying the streamable-HTTP session id.
const SESSION_HEADER: &str = "Mcp-Session-Id";

/// One tool advertised by a connected server (names NOT yet namespaced —
/// the caller decides how to present them to its model).
#[derive(Debug, Clone)]
pub struct ExternalMcpTool {
    pub server: String,
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

enum Transport {
    Stdio {
        _child: Child,
        stdin: ChildStdin,
        stdout: BufReader<ChildStdout>,
    },
    Http {
        client: reqwest::Client,
        url: String,
        headers: Vec<(String, String)>,
        session_id: Option<String>,
    },
}

/// A connected MCP server (handshake already completed).
pub struct McpClient {
    pub server: String,
    transport: Transport,
    next_id: u64,
}

impl McpClient {
    /// Connect to one server entry (the `mcpServers` value shape) and run
    /// the `initialize` handshake.
    pub async fn connect(server: &str, entry: &serde_json::Value) -> anyhow::Result<Self> {
        let transport = if let Some(command) = entry.get("command").and_then(|v| v.as_str()) {
            let args: Vec<String> = entry
                .get("args")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(&args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            if let Some(env) = entry.get("env").and_then(|v| v.as_object()) {
                for (k, v) in env {
                    if let Some(val) = v.as_str() {
                        cmd.env(k, val);
                    }
                }
            }
            let mut child = cmd
                .spawn()
                .map_err(|e| anyhow::anyhow!("could not spawn '{command}': {e}"))?;
            let stdin = child.stdin.take().expect("stdin piped");
            let stdout = BufReader::new(child.stdout.take().expect("stdout piped"));
            Transport::Stdio {
                _child: child,
                stdin,
                stdout,
            }
        } else if let Some(url) = entry.get("url").and_then(|v| v.as_str()) {
            let headers: Vec<(String, String)> = entry
                .get("headers")
                .and_then(|v| v.as_object())
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            Transport::Http {
                client: reqwest::Client::new(),
                url: url.to_string(),
                headers,
                session_id: None,
            }
        } else {
            anyhow::bail!("entry has neither 'command' nor 'url'");
        };

        let mut client = McpClient {
            server: server.to_string(),
            transport,
            next_id: 0,
        };

        let init = client
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "peckboard",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
                SETUP_TIMEOUT,
            )
            .await?;
        // Streamable HTTP servers may mint a session at initialize; nothing
        // else in the response is load-bearing for this minimal client.
        let _ = init;
        client
            .notify("notifications/initialized", serde_json::json!({}))
            .await?;
        Ok(client)
    }

    /// `tools/list`, following pagination cursors.
    pub async fn list_tools(&mut self) -> anyhow::Result<Vec<ExternalMcpTool>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(c) => serde_json::json!({ "cursor": c }),
                None => serde_json::json!({}),
            };
            let result = self.request("tools/list", params, SETUP_TIMEOUT).await?;
            for t in result
                .get("tools")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                let Some(name) = t.get("name").and_then(|v| v.as_str()) else {
                    continue;
                };
                tools.push(ExternalMcpTool {
                    server: self.server.clone(),
                    name: name.to_string(),
                    description: t
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    input_schema: t
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({ "type": "object" })),
                });
            }
            cursor = result
                .get("nextCursor")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok(tools)
    }

    /// `tools/call`. `Ok` carries the joined text content (truncated at
    /// [`MAX_RESULT_CHARS`]); a result flagged `isError` comes back as `Err`
    /// so the caller can feed it to the model as a tool error.
    pub async fn call_tool(
        &mut self,
        tool: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<String> {
        let result = self
            .request(
                "tools/call",
                serde_json::json!({ "name": tool, "arguments": arguments }),
                CALL_TIMEOUT,
            )
            .await?;
        let mut parts: Vec<String> = Vec::new();
        for block in result
            .get("content")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            match block.get("type").and_then(|v| v.as_str()) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                        parts.push(text.to_string());
                    }
                }
                Some(other) => parts.push(format!("[{other} content omitted]")),
                None => {}
            }
        }
        let text = truncate_chars(parts.join("\n"), MAX_RESULT_CHARS);
        if result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            anyhow::bail!(
                "{}",
                if text.is_empty() {
                    "tool error".into()
                } else {
                    text
                }
            );
        }
        Ok(text)
    }

    async fn notify(&mut self, method: &str, params: serde_json::Value) -> anyhow::Result<()> {
        let msg = serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params });
        if let Transport::Stdio { stdin, .. } = &mut self.transport {
            let line = format!("{msg}\n");
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await?;
            return Ok(());
        }
        // Response body (202/200) is irrelevant for a notification.
        self.http_post(&msg).await.map(|_| ())
    }

    async fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> anyhow::Result<serde_json::Value> {
        self.next_id += 1;
        let id = self.next_id;
        let server = self.server.clone();
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        // Decide the transport kind up front — matching on `&mut
        // self.transport` around a `self.…(…).await` call would hold two
        // mutable borrows of `self` at once.
        let response = if matches!(self.transport, Transport::Stdio { .. }) {
            tokio::time::timeout(timeout, self.stdio_roundtrip(&msg, id))
                .await
                .map_err(|_| {
                    anyhow::anyhow!("'{server}': {method} timed out after {timeout:?}")
                })??
        } else {
            let body = tokio::time::timeout(timeout, self.http_post(&msg))
                .await
                .map_err(|_| {
                    anyhow::anyhow!("'{server}': {method} timed out after {timeout:?}")
                })??;
            extract_response_with_id(&body, id)
                .ok_or_else(|| anyhow::anyhow!("no JSON-RPC response for id {id} in reply"))?
        };
        if let Some(err) = response.get("error") {
            let msg = err
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            anyhow::bail!("'{}': {method} failed: {msg}", self.server);
        }
        Ok(response
            .get("result")
            .cloned()
            .unwrap_or(serde_json::json!({})))
    }

    /// Write one request line, then read lines until the matching response
    /// id shows up. Notifications are skipped; server→client requests get a
    /// best-effort reply so a spec-compliant server never stalls on us.
    async fn stdio_roundtrip(
        &mut self,
        msg: &serde_json::Value,
        id: u64,
    ) -> anyhow::Result<serde_json::Value> {
        let Transport::Stdio { stdin, stdout, .. } = &mut self.transport else {
            unreachable!("stdio_roundtrip called on http transport");
        };
        let line = format!("{msg}\n");
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;

        let mut buf = String::new();
        loop {
            buf.clear();
            let n = stdout.read_line(&mut buf).await?;
            if n == 0 {
                anyhow::bail!("server closed stdout");
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(buf.trim()) else {
                continue; // non-JSON noise on stdout
            };
            if value.get("id").and_then(|v| v.as_u64()) == Some(id)
                && (value.get("result").is_some() || value.get("error").is_some())
            {
                return Ok(value);
            }
            // Server→client request: answer it so the server keeps going.
            if let (Some(req_id), Some(method)) = (
                value.get("id").cloned(),
                value.get("method").and_then(|v| v.as_str()),
            ) {
                let reply = if method == "ping" {
                    serde_json::json!({ "jsonrpc": "2.0", "id": req_id, "result": {} })
                } else {
                    serde_json::json!({
                        "jsonrpc": "2.0", "id": req_id,
                        "error": { "code": -32601, "message": "not supported by this client" },
                    })
                };
                let reply_line = format!("{reply}\n");
                stdin.write_all(reply_line.as_bytes()).await?;
                stdin.flush().await?;
            }
            // Anything else (notification, unrelated response): keep reading.
        }
    }

    /// POST one JSON-RPC message; returns the raw response body. Captures
    /// the session id issued at `initialize` and echoes it afterwards.
    async fn http_post(&mut self, msg: &serde_json::Value) -> anyhow::Result<String> {
        let Transport::Http {
            client,
            url,
            headers,
            session_id,
        } = &mut self.transport
        else {
            unreachable!("http_post called on stdio transport");
        };
        let mut req = client
            .post(url.as_str())
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        for (k, v) in headers.iter() {
            req = req.header(k, v);
        }
        if let Some(sid) = session_id.as_deref() {
            req = req.header(SESSION_HEADER, sid);
        }
        let resp = req.json(msg).send().await?;
        if let Some(sid) = resp
            .headers()
            .get(SESSION_HEADER)
            .and_then(|v| v.to_str().ok())
        {
            *session_id = Some(sid.to_string());
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() && status.as_u16() != 202 {
            anyhow::bail!("HTTP {status}: {}", truncate_chars(body, 300));
        }
        Ok(body)
    }
}

/// Pull the JSON-RPC response with `id` out of a raw HTTP body — either a
/// plain JSON document or a `text/event-stream` payload whose `data:` lines
/// carry JSON-RPC messages.
fn extract_response_with_id(body: &str, id: u64) -> Option<serde_json::Value> {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        if value.get("id").and_then(|v| v.as_u64()) == Some(id) {
            return Some(value);
        }
    }
    for line in body.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(data.trim()) {
            if value.get("id").and_then(|v| v.as_u64()) == Some(id) {
                return Some(value);
            }
        }
    }
    None
}

fn truncate_chars(text: String, max: usize) -> String {
    if text.chars().count() <= max {
        return text;
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}\n… [output truncated at {max} characters]")
}

/// All connected clients for one turn, keyed by server name. Failed
/// connections are logged and skipped — the turn runs with whatever
/// connected. Dropping the set kills stdio children (`kill_on_drop`).
pub struct McpClientSet {
    clients: HashMap<String, McpClient>,
}

impl McpClientSet {
    pub async fn connect(entries: &[(String, serde_json::Value)]) -> Self {
        let mut clients = HashMap::new();
        for (name, entry) in entries {
            match McpClient::connect(name, entry).await {
                Ok(client) => {
                    clients.insert(name.clone(), client);
                }
                Err(e) => {
                    tracing::warn!("mcp client: could not connect to '{name}': {e}");
                }
            }
        }
        McpClientSet { clients }
    }

    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    /// Tools from every connected server (failures per server logged).
    pub async fn list_all_tools(&mut self) -> Vec<ExternalMcpTool> {
        let mut all = Vec::new();
        for (name, client) in self.clients.iter_mut() {
            match client.list_tools().await {
                Ok(tools) => all.extend(tools),
                Err(e) => tracing::warn!("mcp client: tools/list failed for '{name}': {e}"),
            }
        }
        all
    }

    pub async fn call(
        &mut self,
        server: &str,
        tool: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<String> {
        let client = self
            .clients
            .get_mut(server)
            .ok_or_else(|| anyhow::anyhow!("no connected MCP server named '{server}'"))?;
        client.call_tool(tool, arguments).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stdio MCP "server" as a shell script: lock-step, id sequence is
    /// deterministic (1 = initialize, notification, 2 = tools/list,
    /// 3 = tools/call), so canned responses per read line suffice.
    fn fake_server_script(dir: &std::path::Path) -> String {
        let script = r#"#!/bin/sh
read line
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"fake","version":"0"}}}'
read line
read line
printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/log","params":{"note":"interleaved noise"}}'
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"echoes","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}}'
read line
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"echoed: hi"}]}}'
read line
"#;
        let path = dir.join("fake-mcp.sh");
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().to_string()
    }

    #[tokio::test]
    async fn stdio_handshake_list_and_call() {
        let tmp = tempfile::tempdir().unwrap();
        let script = fake_server_script(tmp.path());
        let entry = serde_json::json!({ "type": "stdio", "command": script });

        let mut client = McpClient::connect("fake", &entry).await.unwrap();
        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].server, "fake");
        assert_eq!(tools[0].input_schema["type"], "object");

        let out = client
            .call_tool("echo", serde_json::json!({ "text": "hi" }))
            .await
            .unwrap();
        assert_eq!(out, "echoed: hi");
    }

    #[tokio::test]
    async fn connect_failure_is_skipped_by_the_set() {
        let entries = vec![(
            "broken".to_string(),
            serde_json::json!({ "type": "stdio", "command": "/nonexistent/definitely-not-a-binary" }),
        )];
        let set = McpClientSet::connect(&entries).await;
        assert!(set.is_empty());
    }

    #[test]
    fn extracts_response_from_json_and_sse_bodies() {
        let json = r#"{"jsonrpc":"2.0","id":7,"result":{"ok":true}}"#;
        assert_eq!(
            extract_response_with_id(json, 7).unwrap()["result"]["ok"],
            true
        );
        assert!(extract_response_with_id(json, 8).is_none());

        let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":9,\"result\":{\"n\":1}}\n\n";
        assert_eq!(extract_response_with_id(sse, 9).unwrap()["result"]["n"], 1);
    }

    #[test]
    fn truncation_marks_oversized_output() {
        let out = truncate_chars("x".repeat(MAX_RESULT_CHARS + 5), MAX_RESULT_CHARS);
        assert!(out.contains("[output truncated"));
        assert_eq!(truncate_chars("short".into(), MAX_RESULT_CHARS), "short");
    }
}
