use std::process::Stdio;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use crate::db::Db;
use crate::provider::stream::ProviderEvent;
use crate::ws::broadcaster::{Broadcaster, WsEvent};

use super::build_cli_args;
use crate::provider::stream::SpawnConfig;

/// Handle to a running Claude CLI child process.
pub struct ClaudeProcess {
    child: Child,
    session_id: String,
}

impl ClaudeProcess {
    /// Access the session ID associated with this process.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Check whether the child process is still running.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

/// Spawn a Claude CLI child process.
///
/// Builds CLI arguments via `build_cli_args`, sets the working directory,
/// and configures stdin/stdout/stderr pipes.
pub fn spawn_claude(
    session_id: &str,
    message: &str,
    config: &SpawnConfig,
    conversation_id: Option<&str>,
) -> anyhow::Result<ClaudeProcess> {
    let args = build_cli_args(message, config, conversation_id);

    // args[0] is "claude", the rest are actual arguments
    let program = &args[0];
    let cli_args = &args[1..];

    tracing::info!(
        session_id = session_id,
        "Spawning claude process: {} {}",
        program,
        cli_args.join(" ")
    );

    let mut cmd = Command::new(program);
    cmd.args(cli_args)
        .current_dir(&config.working_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Apply any extra environment variables from the config
    for (key, value) in &config.env {
        cmd.env(key, value);
    }

    let child = cmd.spawn().map_err(|e| {
        anyhow::anyhow!(
            "Failed to spawn claude process for session {}: {}",
            session_id,
            e
        )
    })?;

    Ok(ClaudeProcess {
        child,
        session_id: session_id.to_string(),
    })
}

/// Read stdout line-by-line from the Claude CLI process, parse each JSON line
/// into a `ProviderEvent`, persist it to the DB, and broadcast via WebSocket.
///
/// This function consumes the process and runs until the child exits.
/// On exit it emits either a `Completed` or `Crashed` event.
pub async fn stream_events(
    mut process: ClaudeProcess,
    db: Db,
    broadcaster: Arc<Broadcaster>,
) {
    let session_id = process.session_id.clone();

    let stdout = match process.child.stdout.take() {
        Some(s) => s,
        None => {
            tracing::error!(session_id = %session_id, "No stdout handle on claude process");
            emit_event(
                &db,
                &broadcaster,
                &session_id,
                ProviderEvent::Crashed {
                    reason: "no stdout handle".into(),
                    exit_code: None,
                    stderr: None,
                },
            )
            .await;
            return;
        }
    };

    let stderr = process.child.stderr.take();

    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();

    // Track state for mapping stream-json events
    let mut conversation_id: Option<String> = None;
    let mut model_name: Option<String> = None;
    let mut current_tool_id: Option<String> = None;
    let mut emitted_start = false;

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let json: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    session_id = %session_id,
                    "Non-JSON line from claude: {} (error: {})",
                    &line[..line.len().min(200)],
                    e
                );
                continue;
            }
        };

        // Extract events based on the stream-json type field
        let events = parse_stream_json(
            &json,
            &mut conversation_id,
            &mut model_name,
            &mut current_tool_id,
            &mut emitted_start,
        );

        for event in events {
            emit_event(&db, &broadcaster, &session_id, event).await;
        }
    }

    // Wait for the process to exit and capture the exit code
    let exit_status = process.child.wait().await;

    // Read any remaining stderr
    let stderr_text = if let Some(stderr) = stderr {
        let stderr_reader = BufReader::new(stderr);
        let mut buf = String::new();
        let mut lines = stderr_reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if !buf.is_empty() {
                buf.push('\n');
            }
            buf.push_str(&line);
        }
        if buf.is_empty() { None } else { Some(buf) }
    } else {
        None
    };

    let is_completed = match exit_status {
        Ok(status) if status.success() => {
            // Process exited cleanly — emit Completed
            emit_event(
                &db,
                &broadcaster,
                &session_id,
                ProviderEvent::Completed {
                    conversation_id,
                },
            )
            .await;
            true
        }
        Ok(status) => {
            let code = status.code();
            tracing::warn!(
                session_id = %session_id,
                exit_code = ?code,
                "Claude process exited with non-zero status"
            );
            emit_event(
                &db,
                &broadcaster,
                &session_id,
                ProviderEvent::Crashed {
                    reason: format!("process exited with code {}", code.unwrap_or(-1)),
                    exit_code: code,
                    stderr: stderr_text,
                },
            )
            .await;
            false
        }
        Err(e) => {
            tracing::error!(
                session_id = %session_id,
                "Failed to wait for claude process: {}",
                e
            );
            emit_event(
                &db,
                &broadcaster,
                &session_id,
                ProviderEvent::Crashed {
                    reason: format!("wait error: {}", e),
                    exit_code: None,
                    stderr: stderr_text,
                },
            )
            .await;
            false
        }
    };

    // Auto-deliver queued message if one exists after successful completion
    if is_completed {
        if let Ok(Some(queued)) = db.get_queued_message(&session_id).await {
            let _ = db.delete_queued_message(&session_id).await;
            tracing::info!(
                session_id = %session_id,
                "Found queued message after completion, broadcasting notification"
            );
            // Append the queued message as a user event so the frontend sees it
            if let Ok(user_ev) = db
                .append_event(
                    &session_id,
                    "user",
                    serde_json::json!({"text": queued.text}),
                )
                .await
            {
                broadcaster.broadcast(WsEvent {
                    event_type: "event".into(),
                    session_id: session_id.clone(),
                    data: serde_json::json!({
                        "id": user_ev.id,
                        "seq": user_ev.seq,
                        "ts": user_ev.ts,
                        "kind": user_ev.kind,
                        "data": serde_json::from_str::<serde_json::Value>(&user_ev.data).unwrap_or_default(),
                    }),
                });
            }
            // Broadcast a system notification so the frontend knows to re-spawn
            emit_event(
                &db,
                &broadcaster,
                &session_id,
                ProviderEvent::ControlRequest {
                    request_id: uuid::Uuid::new_v4().to_string(),
                    request_type: "queued-message-ready".into(),
                    payload: serde_json::json!({"text": queued.text}),
                },
            )
            .await;
        }
    }
}

/// Parse a single JSON line from Claude CLI stream-json output into zero or more
/// `ProviderEvent` values.
///
/// The Claude CLI `--output-format stream-json --verbose` emits JSON objects that
/// can take several forms. We handle the common patterns:
///
/// - `{"type":"system","subtype":"init",...}` - initialization with model info
/// - `{"type":"assistant","message":{...}}` - complete message snapshots
/// - `{"type":"content_block_start","content_block":{...}}` - start of a content block
/// - `{"type":"content_block_delta","delta":{...}}` - streamed text chunk
/// - `{"type":"content_block_stop"}` - end of a content block
/// - `{"type":"message_start","message":{...}}` - message start with metadata
/// - `{"type":"message_delta","delta":{...}}` - message-level delta (e.g. stop_reason)
/// - `{"type":"message_stop"}` - message completed
/// - `{"type":"result",...}` - final result with conversation_id
fn parse_stream_json(
    json: &serde_json::Value,
    conversation_id: &mut Option<String>,
    model_name: &mut Option<String>,
    current_tool_id: &mut Option<String>,
    emitted_start: &mut bool,
) -> Vec<ProviderEvent> {
    let mut events = Vec::new();

    let msg_type = json
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    match msg_type {
        // ── system init ──────────────────────────────────────────
        "system" => {
            let subtype = json
                .get("subtype")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            if subtype == "init" {
                if let Some(model) = json.get("model").and_then(|v| v.as_str()) {
                    *model_name = Some(model.to_string());
                }
                if let Some(cid) = json.get("conversation_id").and_then(|v| v.as_str()) {
                    *conversation_id = Some(cid.to_string());
                }
                if !*emitted_start {
                    *emitted_start = true;
                    events.push(ProviderEvent::Started {
                        model: model_name.clone().unwrap_or_else(|| "unknown".into()),
                        conversation_id: conversation_id.clone(),
                        metadata: json.clone(),
                    });
                }
            }
        }

        // ── message_start ────────────────────────────────────────
        "message_start" => {
            if let Some(msg) = json.get("message") {
                if let Some(model) = msg.get("model").and_then(|v| v.as_str()) {
                    *model_name = Some(model.to_string());
                }
                if let Some(cid) = msg.get("id").and_then(|v| v.as_str()) {
                    // The message id can serve as a conversation identifier
                    if conversation_id.is_none() {
                        *conversation_id = Some(cid.to_string());
                    }
                }
                if !*emitted_start {
                    *emitted_start = true;
                    events.push(ProviderEvent::Started {
                        model: model_name.clone().unwrap_or_else(|| "unknown".into()),
                        conversation_id: conversation_id.clone(),
                        metadata: json.clone(),
                    });
                }
            }
        }

        // ── content_block_start ──────────────────────────────────
        "content_block_start" => {
            if let Some(block) = json.get("content_block") {
                let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if block_type == "tool_use" {
                    let tool_id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    *current_tool_id = Some(tool_id.clone());
                    events.push(ProviderEvent::ToolStart {
                        tool_use_id: tool_id,
                        name,
                        input: serde_json::Value::Object(serde_json::Map::new()),
                    });
                }
            }
        }

        // ── content_block_delta ──────────────────────────────────
        "content_block_delta" => {
            if let Some(delta) = json.get("delta") {
                let delta_type = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match delta_type {
                    "text_delta" => {
                        if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                            if !text.is_empty() {
                                events.push(ProviderEvent::Text {
                                    text: text.to_string(),
                                });
                            }
                        }
                    }
                    "input_json_delta" => {
                        // Partial JSON for tool input — we emit as text for now
                        // since the full input comes with content_block_stop or
                        // in the assistant message snapshot
                    }
                    _ => {}
                }
            }
        }

        // ── content_block_stop ───────────────────────────────────
        "content_block_stop" => {
            // If we were tracking a tool, emit tool end
            if let Some(tool_id) = current_tool_id.take() {
                events.push(ProviderEvent::ToolEnd {
                    tool_use_id: tool_id,
                    output: None,
                    error: None,
                });
            }
        }

        // ── assistant message snapshot ───────────────────────────
        "assistant" => {
            if let Some(msg) = json.get("message") {
                if let Some(content) = msg.get("content").and_then(|v| v.as_array()) {
                    for block in content {
                        let block_type =
                            block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        match block_type {
                            "text" => {
                                if let Some(text) =
                                    block.get("text").and_then(|v| v.as_str())
                                {
                                    if !text.is_empty() {
                                        events.push(ProviderEvent::Text {
                                            text: text.to_string(),
                                        });
                                    }
                                }
                            }
                            "tool_use" => {
                                let tool_id = block
                                    .get("id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let name = block
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown")
                                    .to_string();
                                let input = block
                                    .get("input")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null);
                                events.push(ProviderEvent::ToolStart {
                                    tool_use_id: tool_id,
                                    name,
                                    input,
                                });
                            }
                            "tool_result" => {
                                let tool_id = block
                                    .get("tool_use_id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let output = block
                                    .get("content")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string());
                                let is_error = block
                                    .get("is_error")
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false);
                                let error = if is_error {
                                    output.clone()
                                } else {
                                    None
                                };
                                events.push(ProviderEvent::ToolEnd {
                                    tool_use_id: tool_id,
                                    output: if is_error { None } else { output },
                                    error,
                                });
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        // ── result ───────────────────────────────────────────────
        "result" => {
            if let Some(cid) = json.get("conversation_id").and_then(|v| v.as_str()) {
                *conversation_id = Some(cid.to_string());
            }
            // The result event signals completion — we let the process exit
            // handling in stream_events produce the final Completed/Crashed event.
            // But capture the conversation_id for that final event.
        }

        // ── message_delta ────────────────────────────────────────
        "message_delta" => {
            // May contain stop_reason; no action needed for event stream
        }

        // ── message_stop ─────────────────────────────────────────
        "message_stop" => {
            // End of a message turn; completion handled by process exit
        }

        _ => {
            tracing::debug!(
                msg_type = msg_type,
                "Unhandled stream-json type"
            );
        }
    }

    events
}

/// Persist a `ProviderEvent` to the database and broadcast it via WebSocket.
async fn emit_event(
    db: &Db,
    broadcaster: &Broadcaster,
    session_id: &str,
    event: ProviderEvent,
) {
    let kind = event.event_kind().to_string();
    let data = event.event_data();

    match db.append_event(session_id, &kind, data.clone()).await {
        Ok(db_event) => {
            // Update last_activity
            let now = chrono::Utc::now().to_rfc3339();
            let _ = db
                .update_session(
                    session_id,
                    crate::db::models::UpdateSession {
                        last_activity: Some(now),
                        ..Default::default()
                    },
                )
                .await;

            // If this is a Completed event with a conversation_id, persist it on the session
            if let ProviderEvent::Completed {
                conversation_id: Some(ref cid),
            } = event
            {
                let _ = db
                    .update_session(
                        session_id,
                        crate::db::models::UpdateSession {
                            conversation_id: Some(Some(cid.clone())),
                            ..Default::default()
                        },
                    )
                    .await;
            }

            // If this is a Started event with a conversation_id, persist it too
            if let ProviderEvent::Started {
                conversation_id: Some(ref cid),
                ..
            } = event
            {
                let _ = db
                    .update_session(
                        session_id,
                        crate::db::models::UpdateSession {
                            conversation_id: Some(Some(cid.clone())),
                            ..Default::default()
                        },
                    )
                    .await;
            }

            broadcaster.broadcast(WsEvent {
                event_type: "event".into(),
                session_id: session_id.to_string(),
                data: serde_json::json!({
                    "id": db_event.id,
                    "seq": db_event.seq,
                    "ts": db_event.ts,
                    "kind": db_event.kind,
                    "data": serde_json::from_str::<serde_json::Value>(&db_event.data).unwrap_or_default(),
                }),
            });
        }
        Err(e) => {
            tracing::error!(
                session_id = session_id,
                kind = %kind,
                "Failed to persist event: {}",
                e
            );
        }
    }
}

/// Kill the Claude CLI process. Sends SIGTERM first, waits up to 5 seconds,
/// then sends SIGKILL if the process is still alive.
pub async fn kill_process(mut process: ClaudeProcess) {
    let session_id = process.session_id.clone();

    // Try graceful termination first (SIGTERM on Unix, TerminateProcess on Windows)
    if let Some(id) = process.child.id() {
        tracing::info!(session_id = %session_id, pid = id, "Sending SIGTERM to claude process");

        #[cfg(unix)]
        {
            // Send SIGTERM via nix/libc
            unsafe {
                libc::kill(id as i32, libc::SIGTERM);
            }
        }

        #[cfg(not(unix))]
        {
            let _ = process.child.kill().await;
            return;
        }

        // Wait up to 5 seconds for graceful exit
        let wait_result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            process.child.wait(),
        )
        .await;

        match wait_result {
            Ok(Ok(status)) => {
                tracing::info!(
                    session_id = %session_id,
                    exit_code = ?status.code(),
                    "Claude process terminated gracefully"
                );
                return;
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    session_id = %session_id,
                    "Error waiting for claude process: {}",
                    e
                );
            }
            Err(_) => {
                tracing::warn!(
                    session_id = %session_id,
                    "Claude process did not exit within 5s, sending SIGKILL"
                );
            }
        }
    }

    // Force kill
    if let Err(e) = process.child.kill().await {
        tracing::error!(
            session_id = %session_id,
            "Failed to SIGKILL claude process: {}",
            e
        );
    } else {
        let _ = process.child.wait().await;
        tracing::info!(session_id = %session_id, "Claude process killed");
    }
}

/// Interrupt the Claude CLI process by writing a newline to its stdin.
/// This simulates pressing Enter, which Claude CLI interprets as an interrupt.
pub async fn interrupt_process(process: &mut ClaudeProcess) {
    let session_id = &process.session_id;

    if let Some(stdin) = process.child.stdin.as_mut() {
        match stdin.write_all(b"\n").await {
            Ok(()) => {
                let _ = stdin.flush().await;
                tracing::info!(session_id = %session_id, "Sent interrupt (newline) to claude process");
            }
            Err(e) => {
                tracing::error!(
                    session_id = %session_id,
                    "Failed to write interrupt to claude stdin: {}",
                    e
                );
            }
        }
    } else {
        tracing::warn!(session_id = %session_id, "No stdin handle to send interrupt");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_system_init() {
        let json = serde_json::json!({
            "type": "system",
            "subtype": "init",
            "model": "claude-sonnet-4-20250514",
            "conversation_id": "conv-abc123"
        });

        let mut cid = None;
        let mut model = None;
        let mut tool_id = None;
        let mut started = false;

        let events = parse_stream_json(&json, &mut cid, &mut model, &mut tool_id, &mut started);

        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ProviderEvent::Started { model, .. } if model == "claude-sonnet-4-20250514"));
        assert_eq!(cid.as_deref(), Some("conv-abc123"));
        assert!(started);
    }

    #[test]
    fn test_parse_content_block_delta_text() {
        let json = serde_json::json!({
            "type": "content_block_delta",
            "delta": {
                "type": "text_delta",
                "text": "Hello world"
            }
        });

        let mut cid = None;
        let mut model = None;
        let mut tool_id = None;
        let mut started = true;

        let events = parse_stream_json(&json, &mut cid, &mut model, &mut tool_id, &mut started);

        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ProviderEvent::Text { text } if text == "Hello world"));
    }

    #[test]
    fn test_parse_content_block_start_tool_use() {
        let json = serde_json::json!({
            "type": "content_block_start",
            "content_block": {
                "type": "tool_use",
                "id": "tool_123",
                "name": "Read"
            }
        });

        let mut cid = None;
        let mut model = None;
        let mut tool_id = None;
        let mut started = true;

        let events = parse_stream_json(&json, &mut cid, &mut model, &mut tool_id, &mut started);

        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            ProviderEvent::ToolStart { tool_use_id, name, .. }
            if tool_use_id == "tool_123" && name == "Read"
        ));
        assert_eq!(tool_id.as_deref(), Some("tool_123"));
    }

    #[test]
    fn test_parse_content_block_stop_clears_tool() {
        let json = serde_json::json!({ "type": "content_block_stop" });

        let mut cid = None;
        let mut model = None;
        let mut tool_id = Some("tool_123".to_string());
        let mut started = true;

        let events = parse_stream_json(&json, &mut cid, &mut model, &mut tool_id, &mut started);

        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            ProviderEvent::ToolEnd { tool_use_id, .. } if tool_use_id == "tool_123"
        ));
        assert!(tool_id.is_none());
    }

    #[test]
    fn test_parse_result_captures_conversation_id() {
        let json = serde_json::json!({
            "type": "result",
            "conversation_id": "conv-final-456"
        });

        let mut cid = None;
        let mut model = None;
        let mut tool_id = None;
        let mut started = true;

        let events = parse_stream_json(&json, &mut cid, &mut model, &mut tool_id, &mut started);

        assert!(events.is_empty());
        assert_eq!(cid.as_deref(), Some("conv-final-456"));
    }

    #[test]
    fn test_parse_message_start() {
        let json = serde_json::json!({
            "type": "message_start",
            "message": {
                "id": "msg-abc",
                "model": "claude-sonnet-4-20250514",
                "role": "assistant"
            }
        });

        let mut cid = None;
        let mut model = None;
        let mut tool_id = None;
        let mut started = false;

        let events = parse_stream_json(&json, &mut cid, &mut model, &mut tool_id, &mut started);

        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ProviderEvent::Started { model, .. } if model == "claude-sonnet-4-20250514"));
        assert!(started);
    }

    #[test]
    fn test_parse_ignores_empty_text() {
        let json = serde_json::json!({
            "type": "content_block_delta",
            "delta": {
                "type": "text_delta",
                "text": ""
            }
        });

        let mut cid = None;
        let mut model = None;
        let mut tool_id = None;
        let mut started = true;

        let events = parse_stream_json(&json, &mut cid, &mut model, &mut tool_id, &mut started);
        assert!(events.is_empty());
    }

    #[test]
    fn test_parse_assistant_snapshot() {
        let json = serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [
                    { "type": "text", "text": "Here is the answer." },
                    {
                        "type": "tool_use",
                        "id": "tu_1",
                        "name": "Bash",
                        "input": { "command": "ls" }
                    }
                ]
            }
        });

        let mut cid = None;
        let mut model = None;
        let mut tool_id = None;
        let mut started = true;

        let events = parse_stream_json(&json, &mut cid, &mut model, &mut tool_id, &mut started);

        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], ProviderEvent::Text { text } if text == "Here is the answer."));
        assert!(matches!(
            &events[1],
            ProviderEvent::ToolStart { tool_use_id, name, .. }
            if tool_use_id == "tu_1" && name == "Bash"
        ));
    }

    #[test]
    fn test_no_duplicate_start() {
        let json = serde_json::json!({
            "type": "system",
            "subtype": "init",
            "model": "opus"
        });

        let mut cid = None;
        let mut model = None;
        let mut tool_id = None;
        let mut started = true; // already started

        let events = parse_stream_json(&json, &mut cid, &mut model, &mut tool_id, &mut started);

        // Should not emit another Started event
        assert!(events.is_empty());
        // But should still update model_name
        assert_eq!(model.as_deref(), Some("opus"));
    }
}
