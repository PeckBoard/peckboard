//! Claude CLI subprocess lifecycle: spawn, stream stdout events, deliver
//! stdin lines from peers, and produce a final `Completed`/`Crashed`
//! event on exit. The pure parsing logic lives in [`parser`] and the
//! best-effort path sandbox in [`sandbox`].

mod parser;
mod sandbox;

use std::process::Stdio;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Notify;

use crate::db::Db;
use crate::provider::agent::emit_event;
use crate::provider::stream::ProviderEvent;
use crate::ws::broadcaster::{Broadcaster, WsEvent};

use super::build_cli_args;
use crate::provider::stream::SpawnConfig;
use parser::{normalize_questions, parse_stream_json};
use sandbox::check_path_violation;

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
        .stderr(Stdio::piped())
        // Safety net: if the streaming task is dropped without a clean
        // shutdown (e.g. JoinHandle::abort), the child still gets SIGKILL.
        .kill_on_drop(true);

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
/// This function consumes the process and runs until the child exits or
/// `cancel` is notified. On exit it emits either a `Completed`, `Crashed`,
/// or (when cancelled) a `Crashed { reason: "interrupted" }` event.
///
/// The `stdin_rx` channel allows callers to feed text into the process's
/// stdin (e.g. to deliver answers to questions).
///
/// Returns `true` if the process completed successfully, `false` if it
/// crashed, was interrupted, or encountered an error.
pub async fn stream_events(
    mut process: ClaudeProcess,
    db: Db,
    broadcaster: Arc<Broadcaster>,
    stdin_rx: tokio::sync::mpsc::Receiver<String>,
    _stdin_tx: tokio::sync::mpsc::Sender<String>,
    allowed_dir: String,
    cancel: Arc<Notify>,
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

    // Subscribe to broadcasts for immediate inter-worker message delivery
    let mut broadcast_rx = broadcaster.subscribe_all();

    // Track state for mapping stream-json events
    let mut conversation_id: Option<String> = None;
    let mut model_name: Option<String> = None;
    let mut current_tool_id: Option<String> = None;
    let mut emitted_start = false;
    let mut was_cancelled = false;

    // Track file-modifying tool calls for auto cross-worker notification
    let mut pending_file_changes: Vec<String> = Vec::new();
    let mut pending_tool_names: std::collections::HashMap<String, (String, Option<String>)> =
        std::collections::HashMap::new();

    loop {
        let line = tokio::select! {
            _ = cancel.notified() => {
                tracing::info!(
                    session_id = %session_id,
                    "Cancel signal received; killing claude process"
                );
                was_cancelled = true;
                let _ = process.child.start_kill();
                break;
            }
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
            event = broadcast_rx.recv() => {
                if let Ok(ws_event) = event {
                    if ws_event.session_id != session_id {
                        // Not for us
                    } else if ws_event.event_type == "worker-stdin-deliver" {
                        if let Some(text) = ws_event.data.get("text").and_then(|v| v.as_str()) {
                            tracing::info!(
                                session_id = %session_id,
                                "Delivering inter-worker message to running agent"
                            );
                            write_stdin_line(&mut stdin_pipe, text, &session_id).await;
                        }
                    }
                }
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

            // Normalized todo snapshots derived from TodoWrite tool calls in
            // this batch. Emitted as their own `todo` events after the raw
            // provider events below, so the tool call stays visible AND a
            // provider-agnostic snapshot lands in the log for the UI.
            let mut todo_events: Vec<ProviderEvent> = Vec::new();

            for event in &events {
                // Track file-modifying tools for cross-worker notification
                match event {
                    ProviderEvent::ToolStart {
                        tool_use_id,
                        name,
                        input,
                    } => {
                        if let Some(snapshot) = crate::todo::snapshot_from_tool_call(name, input) {
                            todo_events.push(ProviderEvent::Todo {
                                todos: snapshot.todos,
                            });
                        }
                        let file_path = match name.as_str() {
                            "Write" | "Edit" => input
                                .get("file_path")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string()),
                            _ => None,
                        };
                        pending_tool_names.insert(tool_use_id.clone(), (name.clone(), file_path));
                    }
                    ProviderEvent::ToolEnd {
                        tool_use_id, error, ..
                    } => {
                        // Always drop the pending entry on any end (success or
                        // error) so the post-exit synthetic-end sweep only
                        // covers truly orphaned tools.
                        if let Some((name, file_path)) = pending_tool_names.remove(tool_use_id) {
                            if error.is_none() {
                                if let Some(path) = file_path {
                                    if name == "Write" || name == "Edit" {
                                        pending_file_changes.push(path);
                                    }
                                }
                            }
                        }
                    }
                    // When the agent produces text or ends, flush accumulated file changes
                    ProviderEvent::Text { .. } | ProviderEvent::Completed { .. } => {
                        if !pending_file_changes.is_empty() {
                            let changes = std::mem::take(&mut pending_file_changes);
                            if let Ok(Some(session)) = db.get_session(&session_id).await {
                                if session.is_worker {
                                    if let Some(ref project_id) = session.project_id {
                                        // Check if auto_notify_changes is enabled for this project
                                        let auto_notify = db
                                            .get_project(project_id)
                                            .await
                                            .ok()
                                            .flatten()
                                            .map(|p| p.auto_notify_changes)
                                            .unwrap_or(true);

                                        if auto_notify {
                                            let card_title =
                                                if let Some(ref card_id) = session.card_id {
                                                    db.get_card(card_id)
                                                        .await
                                                        .ok()
                                                        .flatten()
                                                        .map(|c| c.title)
                                                } else {
                                                    None
                                                };

                                            if let Ok(workers) =
                                                db.list_worker_sessions_by_project(project_id).await
                                            {
                                                let msg = format!(
                                                    "[Auto] Worker on \"{}\" modified: {}",
                                                    card_title.as_deref().unwrap_or("unknown"),
                                                    changes.join(", ")
                                                );
                                                for ws in &workers {
                                                    if ws.id == session_id {
                                                        continue;
                                                    }
                                                    if let Some(ref cid) = ws.card_id {
                                                        if let Ok(Some(c)) = db.get_card(cid).await
                                                        {
                                                            if c.step == "done"
                                                                || c.step == "wont_do"
                                                            {
                                                                continue;
                                                            }
                                                        }
                                                    }
                                                    // Persist as user event
                                                    let _ = db
                                                        .append_event(
                                                            &ws.id,
                                                            "user",
                                                            serde_json::json!({
                                                                "text": msg,
                                                                "source": "worker-auto-notify",
                                                            }),
                                                        )
                                                        .await;
                                                    // Broadcast for immediate stdin delivery
                                                    broadcaster.broadcast(WsEvent {
                                                        event_type: "worker-stdin-deliver".into(),
                                                        session_id: ws.id.clone(),
                                                        data: serde_json::json!({ "text": msg }),
                                                    });
                                                }
                                                tracing::info!(
                                                    session_id = %session_id,
                                                    files = ?changes,
                                                    "Auto-notified workers of file changes"
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }

            for event in events {
                emit_event(&db, &broadcaster, &session_id, event).await;
            }

            for event in todo_events {
                emit_event(&db, &broadcaster, &session_id, event).await;
            }
        } // end inner block
    } // end loop

    // Drop stdin pipe to signal EOF to the child process
    drop(stdin_pipe);

    // Wait for the process to exit and capture the exit code
    let exit_status = process.child.wait().await;

    // Close any tools we saw start but never saw end. If the CLI exits
    // (cleanly or otherwise) with outstanding tool_use blocks, the user
    // would otherwise see a spinner that never resolves. Emit synthetic
    // ToolEnd events so the event log is self-consistent.
    if !pending_tool_names.is_empty() {
        tracing::warn!(
            session_id = %session_id,
            count = pending_tool_names.len(),
            "CLI exited with open tool_use blocks; emitting synthetic ToolEnd"
        );
        let orphans: Vec<String> = pending_tool_names.keys().cloned().collect();
        for tool_use_id in orphans {
            emit_event(
                &db,
                &broadcaster,
                &session_id,
                ProviderEvent::ToolEnd {
                    tool_use_id,
                    output: None,
                    error: Some("tool did not return a result before agent ended".into()),
                },
            )
            .await;
        }
    }

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

    let is_completed = if was_cancelled {
        emit_event(
            &db,
            &broadcaster,
            &session_id,
            ProviderEvent::Crashed {
                reason: "interrupted".into(),
                exit_code: exit_status.ok().and_then(|s| s.code()),
                stderr: stderr_text,
            },
        )
        .await;
        false
    } else {
        match exit_status {
            Ok(status) if status.success() => {
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
        }
    };

    // Queue draining is handled centrally by the completion listener in
    // main.rs — see SessionManager::drain_queued_message. The provider
    // intentionally does NOT touch the queue so the drain happens on every
    // termination path (success, crash, or interrupt), not just clean exits.

    is_completed
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
