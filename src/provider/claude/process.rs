use std::process::Stdio;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use crate::db::Db;
use crate::provider::agent::emit_event;
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
///
/// The `stdin_rx` channel allows callers to feed text into the process's
/// stdin (e.g. to deliver answers to questions).
///
/// Returns `true` if the process completed successfully, `false` if it
/// crashed or encountered an error.
pub async fn stream_events(
    mut process: ClaudeProcess,
    db: Db,
    broadcaster: Arc<Broadcaster>,
    stdin_rx: tokio::sync::mpsc::Receiver<String>,
    _stdin_tx: tokio::sync::mpsc::Sender<String>,
    allowed_dir: String,
) -> bool {
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
            return false;
        }
    };

    // Take the stdin handle — we'll write to it directly from the stream loop
    // for control_responses (low latency) and via the channel for external
    // callers (e.g. question-resolved route handler).
    let mut stdin_pipe = process.child.stdin.take();

    let stderr = process.child.stderr.take();

    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();
    let mut stdin_rx = stdin_rx;

    // Track state for mapping stream-json events
    let mut conversation_id: Option<String> = None;
    let mut model_name: Option<String> = None;
    let mut current_tool_id: Option<String> = None;
    let mut emitted_start = false;

    loop {
        let line = tokio::select! {
            result = lines.next_line() => {
                match result {
                    Ok(Some(line)) => line,
                    _ => break, // stdout closed
                }
            }
            Some(text) = stdin_rx.recv() => {
                // External caller (e.g. question-resolved) wants to write to stdin
                write_stdin_line(&mut stdin_pipe, &text, &session_id).await;
                continue;
            }
        };

        {
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

            // Handle control_request events directly (need stdin access for auto-allow)
            if json.get("type").and_then(|v| v.as_str()) == Some("control_request") {
                let request = json.get("request");
                let request_id = json
                    .get("request_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let subtype = request
                    .and_then(|r| r.get("subtype"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let tool_name = request
                    .and_then(|r| r.get("tool_name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                if subtype == "can_use_tool" && tool_name == "AskUserQuestion" {
                    // Parse and normalize questions from the input
                    let input = request.and_then(|r| r.get("input"));
                    let tool_use_id = request
                        .and_then(|r| r.get("tool_use_id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let questions = normalize_questions(input);

                    let event_data = serde_json::json!({
                        "requestId": request_id,
                        "toolUseId": tool_use_id,
                        "questions": questions,
                    });

                    // Emit as a "question" event
                    if let Ok(db_event) = db
                        .append_event(&session_id, "question", event_data.clone())
                        .await
                    {
                        let now = chrono::Utc::now().to_rfc3339();
                        let _ = db
                            .update_session(
                                &session_id,
                                crate::db::models::UpdateSession {
                                    last_activity: Some(now),
                                    ..Default::default()
                                },
                            )
                            .await;
                        broadcaster.broadcast(WsEvent {
                            event_type: "event".into(),
                            session_id: session_id.clone(),
                            data: serde_json::json!({
                                "id": db_event.id,
                                "seq": db_event.seq,
                                "ts": db_event.ts,
                                "kind": "question",
                                "data": event_data,
                            }),
                        });
                    }
                } else if subtype == "can_use_tool" {
                    let input = request
                        .and_then(|r| r.get("input"))
                        .cloned()
                        .unwrap_or(serde_json::json!({}));

                    // Check if the tool is trying to access paths outside the allowed directory
                    let denied = check_path_violation(tool_name, &input, &allowed_dir);

                    let frame = if let Some(reason) = denied {
                        tracing::warn!(
                            session_id = %session_id,
                            tool = tool_name,
                            "Denied tool use: {}",
                            reason
                        );
                        serde_json::json!({
                            "type": "control_response",
                            "response": {
                                "subtype": "success",
                                "request_id": request_id,
                                "response": {
                                    "behavior": "deny",
                                    "message": reason,
                                }
                            }
                        })
                    } else {
                        serde_json::json!({
                            "type": "control_response",
                            "response": {
                                "subtype": "success",
                                "request_id": request_id,
                                "response": {
                                    "behavior": "allow",
                                    "updatedInput": input,
                                }
                            }
                        })
                    };
                    write_stdin_line(&mut stdin_pipe, &frame.to_string(), &session_id).await;
                }
                continue;
            }

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
        } // end inner block
    } // end loop

    // Drop stdin pipe to signal EOF to the child process
    drop(stdin_pipe);

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
                ProviderEvent::Completed { conversation_id },
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

    is_completed
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

    let msg_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match msg_type {
        // ── system init ──────────────────────────────────────────
        "system" => {
            let subtype = json.get("subtype").and_then(|v| v.as_str()).unwrap_or("");

            if subtype == "init" {
                if let Some(model) = json.get("model").and_then(|v| v.as_str()) {
                    *model_name = Some(model.to_string());
                }
                // CLI uses "session_id" as the resumable conversation identifier
                if let Some(cid) = json
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .or_else(|| json.get("conversation_id").and_then(|v| v.as_str()))
                {
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
                        let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        match block_type {
                            "text" => {
                                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
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
                                let error = if is_error { output.clone() } else { None };
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

        // ── user message (contains tool results) ─────────────────
        "user" => {
            if let Some(msg) = json.get("message") {
                if let Some(content) = msg.get("content").and_then(|v| v.as_array()) {
                    for block in content {
                        let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        if block_type == "tool_result" {
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
                            let error = if is_error { output.clone() } else { None };
                            events.push(ProviderEvent::ToolEnd {
                                tool_use_id: tool_id,
                                output: if is_error { None } else { output },
                                error,
                            });
                        }
                    }
                }
            }
        }

        // ── result ───────────────────────────────────────────────
        "result" => {
            // CLI uses "session_id" in result events
            if let Some(cid) = json
                .get("session_id")
                .and_then(|v| v.as_str())
                .or_else(|| json.get("conversation_id").and_then(|v| v.as_str()))
            {
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
            tracing::debug!(msg_type = msg_type, "Unhandled stream-json type");
        }
    }

    events
}

/// Write a line to the child process's stdin pipe.
async fn write_stdin_line(
    stdin: &mut Option<tokio::process::ChildStdin>,
    text: &str,
    session_id: &str,
) {
    if let Some(pipe) = stdin.as_mut() {
        if let Err(e) = pipe.write_all(text.as_bytes()).await {
            tracing::warn!(session_id = %session_id, "Failed to write to stdin: {e}");
            return;
        }
        if let Err(e) = pipe.write_all(b"\n").await {
            tracing::warn!(session_id = %session_id, "Failed to write newline to stdin: {e}");
            return;
        }
        if let Err(e) = pipe.flush().await {
            tracing::warn!(session_id = %session_id, "Failed to flush stdin: {e}");
        }
    } else {
        tracing::warn!(session_id = %session_id, "No stdin pipe available");
    }
}

/// Check if a tool's input references paths outside the allowed directory.
/// Returns Some(reason) if the tool should be denied, None if allowed.
fn check_path_violation(
    tool_name: &str,
    input: &serde_json::Value,
    allowed_dir: &str,
) -> Option<String> {
    if allowed_dir.is_empty() {
        return None;
    }

    let allowed = match std::path::Path::new(allowed_dir).canonicalize() {
        Ok(p) => p,
        Err(_) => return None, // Can't resolve allowed dir, skip check
    };

    // Extract file paths from tool input based on tool name
    let paths_to_check: Vec<String> = match tool_name {
        "Read" | "Write" | "Edit" => {
            let mut paths = Vec::new();
            if let Some(p) = input.get("file_path").and_then(|v| v.as_str()) {
                paths.push(p.to_string());
            }
            paths
        }
        "Glob" | "Grep" => {
            let mut paths = Vec::new();
            if let Some(p) = input.get("path").and_then(|v| v.as_str()) {
                paths.push(p.to_string());
            }
            paths
        }
        "Bash" => {
            // For Bash, check the command for obvious path references
            // This is a best-effort check — can't fully parse shell commands
            if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
                // Check for cd to outside directory
                let suspicious_patterns = ["cd /", "cd ~/", "cd ..", "rm -rf /", "cat /etc"];
                for pattern in &suspicious_patterns {
                    if cmd.contains(pattern) {
                        // Try to extract the target path from cd commands
                        if cmd.starts_with("cd ") {
                            let target = cmd
                                .trim_start_matches("cd ")
                                .split_whitespace()
                                .next()
                                .unwrap_or("");
                            if !target.is_empty() {
                                let target_path = if target.starts_with('/') {
                                    std::path::PathBuf::from(target)
                                } else {
                                    std::path::Path::new(allowed_dir).join(target)
                                };
                                if let Ok(resolved) = target_path.canonicalize() {
                                    if !resolved.starts_with(&allowed) {
                                        return Some(format!(
                                            "Access denied: path '{}' is outside the project folder '{}'",
                                            target, allowed_dir
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            return None; // Bash commands are complex; allow unless clearly violating
        }
        "NotebookEdit" => {
            let mut paths = Vec::new();
            if let Some(p) = input.get("notebook_path").and_then(|v| v.as_str()) {
                paths.push(p.to_string());
            }
            paths
        }
        _ => return None, // Unknown tool, allow
    };

    for path_str in &paths_to_check {
        let path = std::path::Path::new(path_str);
        // Resolve relative paths against the allowed directory
        let resolved = if path.is_absolute() {
            match path.canonicalize() {
                Ok(p) => p,
                // File may not exist yet (Write), resolve parent
                Err(_) => {
                    if let Some(parent) = path.parent() {
                        match parent.canonicalize() {
                            Ok(p) => p.join(path.file_name().unwrap_or_default()),
                            Err(_) => path.to_path_buf(),
                        }
                    } else {
                        path.to_path_buf()
                    }
                }
            }
        } else {
            // Relative paths are relative to working dir — should be within allowed
            match std::path::Path::new(allowed_dir).join(path).canonicalize() {
                Ok(p) => p,
                Err(_) => continue, // Can't resolve, allow
            }
        };

        if !resolved.starts_with(&allowed) {
            return Some(format!(
                "Access denied: path '{}' is outside the project folder '{}'",
                path_str, allowed_dir
            ));
        }
    }

    None
}

/// Normalize the questions array from an AskUserQuestion control_request input.
///
/// The CLI sends questions as:
/// ```json
/// { "questions": [{ "question": "...", "header": "...", "multiSelect": false,
///     "options": [{ "label": "A", "description": "..." }] }] }
/// ```
///
/// We normalize options to simple label strings for the frontend, preserving
/// the full structure for the control_response answer frame.
fn normalize_questions(input: Option<&serde_json::Value>) -> serde_json::Value {
    let empty = serde_json::json!([]);
    let raw_questions = match input
        .and_then(|i| i.get("questions"))
        .and_then(|q| q.as_array())
    {
        Some(q) => q,
        None => return empty,
    };

    let mut result = Vec::new();
    for q in raw_questions {
        let question_text = match q.get("question").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => continue,
        };

        let header = q.get("header").and_then(|v| v.as_str());
        let multi_select = q
            .get("multiSelect")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let mut option_labels = Vec::new();
        let mut option_objects = Vec::new();
        if let Some(options) = q.get("options").and_then(|v| v.as_array()) {
            for opt in options {
                if let Some(label) = opt.get("label").and_then(|v| v.as_str()) {
                    option_labels.push(serde_json::Value::String(label.to_string()));
                    option_objects.push(opt.clone());
                }
            }
        }

        let mut entry = serde_json::json!({
            "question": question_text,
            "multiSelect": multi_select,
            "options": option_labels,
            "optionObjects": option_objects,
        });
        if let Some(h) = header {
            entry["header"] = serde_json::Value::String(h.to_string());
        }
        result.push(entry);
    }

    serde_json::Value::Array(result)
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
        let wait_result =
            tokio::time::timeout(std::time::Duration::from_secs(5), process.child.wait()).await;

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
            "session_id": "conv-abc123"
        });

        let mut cid = None;
        let mut model = None;
        let mut tool_id = None;
        let mut started = false;

        let events = parse_stream_json(&json, &mut cid, &mut model, &mut tool_id, &mut started);

        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], ProviderEvent::Started { model, .. } if model == "claude-sonnet-4-20250514")
        );
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
            "session_id": "conv-final-456"
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
        assert!(
            matches!(&events[0], ProviderEvent::Started { model, .. } if model == "claude-sonnet-4-20250514")
        );
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
        assert!(
            matches!(&events[0], ProviderEvent::Text { text } if text == "Here is the answer.")
        );
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
