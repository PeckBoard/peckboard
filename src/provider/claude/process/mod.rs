//! Long-lived Claude CLI subprocess: spawn once per session, stream
//! stdout events, accept new user messages on stdin mid-turn, and
//! synthesize per-turn `Completed`/`Crashed` events from the CLI's
//! `result` frames (and process exit, when it actually dies).
//!
//! # Architecture: long-lived process, multiple turns
//!
//! The CLI is spawned with `--input-format=stream-json
//! --output-format=stream-json`. In that mode it reads JSON envelopes
//! from stdin one line at a time and emits a stream of events on
//! stdout. Each `{"type":"user","message":{role,content}}` line is a
//! fresh user turn; the CLI consumes it whenever it's ready (i.e.
//! after the in-flight `result` event for any prior turn) and
//! responds with the usual `system.init` → `assistant`* → `result`
//! sequence.
//!
//! The peckboard process loop is therefore **per-session**, not
//! per-turn. We map the CLI's `result` event to a peckboard
//! `Completed` (carrying the `conversation_id`) and reset the
//! `turn_active` flag, but the child keeps running. The next
//! `send_message` writes another user envelope and a new turn
//! begins. A message that arrives mid-turn is forwarded straight to
//! stdin; the CLI buffers it and consumes it after the current
//! `result`. That's the mid-stream injection contract — there is no
//! peckboard-layer queue, the CLI is the queue.
//!
//! Process death is the only thing that produces a `Crashed`. If the
//! CLI dies between turns the next `send_message` respawns with
//! `--resume <conv_id>` so the conversation continues seamlessly.
//!
//! The pure parsing logic lives in [`parser`] and the best-effort
//! path sandbox in [`sandbox`].

mod parser;
mod sandbox;
mod usage;

use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Notify;
use tokio::sync::mpsc;

use crate::db::Db;
use crate::provider::agent::emit_event;
use crate::provider::message::UserMessage;
use crate::provider::stream::ProviderEvent;
use crate::ws::broadcaster::{Broadcaster, WsEvent};

use super::build_cli_args;
use crate::provider::stream::SpawnConfig;
use parser::{normalize_questions, parse_stream_json};
use sandbox::check_path_violation;
use usage::{TurnModelUsage, UsageTracker};

/// One message bound for the CLI's stdin.
pub enum StdinMsg {
    /// Start a new user turn. Carries the prompt body and any
    /// attachments; the loop wraps both in the stream-json envelope
    /// (string content for text-only turns, `[image…, text]` content
    /// blocks when attachments are present) before writing.
    /// Setting the turn-active flag is delegated to the loop so the
    /// "is mid-turn" bit only flips once the bytes have actually
    /// reached the child.
    UserTurn(UserMessage),
    /// Write an already-formed JSON line verbatim (control_response
    /// frame, worker-comms delivery, etc.). Doesn't touch the
    /// turn-active flag.
    RawLine(String),
    /// Request a graceful exit once the in-flight turn finishes. The
    /// loop sets a local flag, finishes draining stdout up to and
    /// including the next `result` event, then drops the stdin pipe
    /// so the CLI sees EOF and exits cleanly — no `Crashed` event.
    /// Used by the MCP terminal-step tools (`finish_card`,
    /// `complete_step`, `wont_do_card`) so the tool response can
    /// reach the agent before the process winds down.
    ShutdownAfterTurn,
}

/// Handle to a running Claude CLI child process.
pub struct ClaudeProcess {
    child: Child,
    session_id: String,
}

impl ClaudeProcess {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Construct a `ClaudeProcess` from a pre-built `Command` instead of
    /// going through `spawn_claude`. The default spawn path bakes in the
    /// `claude` binary and stream-json arguments; tests that need a
    /// stand-in subprocess (e.g. `cat` for an echo loop) reach for this.
    /// Only compiled in test builds — production paths always go through
    /// `spawn_claude`.
    #[cfg(test)]
    pub fn from_command_for_test(
        mut cmd: Command,
        session_id: &str,
    ) -> anyhow::Result<ClaudeProcess> {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!("Failed to spawn test subprocess for {session_id}: {e}")
        })?;
        Ok(ClaudeProcess {
            child,
            session_id: session_id.to_string(),
        })
    }
}

/// Spawn a Claude CLI child process for `session_id`.
///
/// Builds CLI arguments via `build_cli_args`, sets the working
/// directory, and configures stdin/stdout/stderr pipes. The first
/// user message is delivered separately, via `stdin_rx`, so the same
/// spawn path works for "first turn" and "resume after idle reaper
/// killed the prior child."
pub fn spawn_claude(
    session_id: &str,
    config: &SpawnConfig,
    conversation_id: Option<&str>,
) -> anyhow::Result<ClaudeProcess> {
    // Wire the SubagentStart hook: write the static context file next to
    // the per-session MCP configs; build_cli_args folds the hook into the
    // merged --settings value. Best-effort — a write failure must never
    // block the spawn, it just loses subagent prompt injection.
    let subagent_context = config
        .mcp_config_path
        .as_deref()
        .map(std::path::Path::new)
        .and_then(std::path::Path::parent)
        .and_then(|dir| match super::write_subagent_context_file(dir) {
            Ok(p) => Some(p.to_string_lossy().into_owned()),
            Err(e) => {
                tracing::warn!(
                    session_id = session_id,
                    "subagent hook context not written, spawning without it: {e}"
                );
                None
            }
        });
    let args = build_cli_args(config, conversation_id, subagent_context.as_deref());

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

    apply_config_env(&mut cmd, &config.env);

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

/// Layer `env` over the child's inherited environment. When the map
/// carries an injected account credential, the inherited credential vars
/// are masked first: the spawn must authenticate as exactly the selected
/// account, and a stray ANTHROPIC_API_KEY exported in peckboard's own
/// shell would otherwise outrank the account's CLAUDE_CODE_OAUTH_TOKEN
/// inside the CLI and silently bill the wrong account. Default-account
/// spawns (no injected credential) keep the inherited host credentials —
/// that is their contract.
fn apply_config_env(cmd: &mut Command, env: &std::collections::HashMap<String, String>) {
    if env.contains_key("ANTHROPIC_API_KEY") || env.contains_key("CLAUDE_CODE_OAUTH_TOKEN") {
        cmd.env_remove("ANTHROPIC_API_KEY");
        cmd.env_remove("CLAUDE_CODE_OAUTH_TOKEN");
    }
    for (key, value) in env {
        cmd.env(key, value);
    }
}
/// State shared with the streaming loop for mid-turn coordination.
///
/// These flags read from outside the loop (e.g. `is_running` on the
/// provider, the idle reaper) so they live in `Arc` instead of as
/// loop-local mutables.
pub struct LoopState {
    /// True while a turn is in flight (between writing a `user`
    /// frame and seeing the matching `result`). Drives
    /// `AgentProvider::is_running`.
    pub turn_active: Arc<AtomicBool>,
    /// Epoch milliseconds of the most recent event from the CLI.
    /// The idle reaper uses this to decide when a quiet, alive
    /// process has been around long enough to recycle.
    pub last_activity: Arc<AtomicU64>,
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Read stdout line-by-line from the Claude CLI process, parse each
/// JSON line into a `ProviderEvent`, persist it to the DB, and
/// broadcast via WebSocket.
///
/// This loop runs for the **lifetime of the child process**, not per
/// turn. A CLI `result` event is mapped to a peckboard `Completed`
/// event (and clears `turn_active`) but the child keeps running and
/// waits for the next user envelope on stdin.
///
/// `stdin_rx` is the channel callers use to deliver new turns (or
/// raw JSON lines like control_response frames). The loop exits when
/// `cancel` is notified, when stdin closes, or when the child exits
/// on its own. If the child exits while `turn_active` is set we
/// synthesize a `Crashed { reason: "interrupted" | "exit-mid-turn"
/// }` event so the UI doesn't show a hung spinner.
///
/// Returns a [`StreamOutcome`]: `completed` is `true` only if the process
/// ended after a clean `Completed` — no mid-turn death AND no error
/// `result` on the final turn. `error` carries the CLI-reported error text
/// (e.g. an expired login's 401) so the handover/compaction completion
/// listener can abort with a user-visible reason instead of finalizing a
/// doc turn that produced no doc.
pub async fn stream_events(
    mut process: ClaudeProcess,
    db: Db,
    broadcaster: Arc<Broadcaster>,
    mut stdin_rx: mpsc::Receiver<StdinMsg>,
    allowed_dir: String,
    cancel: Arc<Notify>,
    state: LoopState,
) -> StreamOutcome {
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
            state.turn_active.store(false, Ordering::Release);
            return StreamOutcome {
                completed: false,
                error: Some("no stdout handle".into()),
            };
        }
    };

    let mut stdin_pipe = process.child.stdin.take();
    let stderr = process.child.stderr.take();

    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();

    // Subscribe to broadcasts for immediate inter-worker message delivery
    let mut broadcast_rx = broadcaster.subscribe_all();

    // Per-conversation state. These cursors persist across turns —
    // a `result` event resets the per-turn flags below but keeps the
    // conversation_id and model_name we've discovered.
    let mut conversation_id: Option<String> = None;
    let mut model_name: Option<String> = None;

    // Per-turn state. The parser uses `emitted_start` to suppress
    // duplicate `Started` events within one turn; we reset it on each
    // `result` so the next turn's `system.init` emits a fresh
    // agent-start event. `current_tool_id` is per-turn for the same
    // reason — a long-running process must not carry over a tool id
    // from a previous turn's content_block_start that never saw its
    // matching content_block_stop because the parser sees them all
    // in order anyway.
    let mut current_tool_id: Option<String> = None;
    let mut emitted_start = false;

    let mut was_cancelled = false;
    let mut saw_clean_completion = false;
    // Error text from the most recent `result` event, when it reported a
    // failure (`is_error: true` / non-"success" subtype — e.g. an expired
    // login's 401). A result line still settles the turn (Completed is
    // emitted, spinners stop), but the exit path must NOT report a clean
    // completion for it: a handover/compaction doc turn that errored has
    // produced no doc, and finalizing it would discard real context in
    // exchange for an error message.
    let mut last_result_error: Option<String> = None;
    // Set when a `ShutdownAfterTurn` arrives on `stdin_rx`. After the
    // current `result` event we break out of the select loop without
    // killing the child; dropping stdin gives it a clean EOF and the
    // exit path produces no Crashed event (turn idle + clean
    // completion). Race-wise the flag is checked AFTER each `result`,
    // so a tool handler that sets the flag and returns immediately is
    // guaranteed to still let its own response — and any follow-on
    // assistant text — reach stdout before the loop tears down.
    let mut shutdown_after_turn = false;

    // Per-process token accounting: diffs the result event's cumulative
    // `modelUsage` into per-turn per-model rows, and accumulates per-message
    // usage so a crashed turn still settles its tokens.
    let mut usage_tracker = UsageTracker::default();

    // Track file-modifying tool calls for auto cross-worker notification
    let mut pending_file_changes: Vec<String> = Vec::new();
    let mut pending_tool_names: std::collections::HashMap<String, (String, Option<String>)> =
        std::collections::HashMap::new();

    // Assembles the CLI's task tools (TaskCreate/TaskUpdate, plus legacy
    // TodoWrite) into replace-all `todo` snapshots. Seeded from the
    // persisted list so a process respawn mid-conversation keeps task ids
    // aligned with the CLI's sequential counter.
    let mut task_tracker = crate::todo::TaskTracker::seed(
        db.list_session_todos(&session_id).await.unwrap_or_default(),
    );

    loop {
        let event_source = tokio::select! {
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
                    Ok(Some(line)) => EventSource::Stdout(line),
                    Ok(None) => break, // stdout closed
                    Err(e) => {
                        tracing::warn!(session_id = %session_id, "stdout read error: {e}");
                        break;
                    }
                }
            }
            msg = stdin_rx.recv() => {
                match msg {
                    Some(StdinMsg::UserTurn(message)) => {
                        let frame = super::build_user_message_frame(&message);
                        if write_stdin_line(&mut stdin_pipe, &frame, &session_id).await {
                            state.turn_active.store(true, Ordering::Release);
                            state.last_activity.store(now_ms(), Ordering::Release);
                        }
                        continue;
                    }
                    Some(StdinMsg::RawLine(line)) => {
                        write_stdin_line(&mut stdin_pipe, &line, &session_id).await;
                        continue;
                    }
                    Some(StdinMsg::ShutdownAfterTurn) => {
                        tracing::info!(
                            session_id = %session_id,
                            "Graceful shutdown requested; will exit after current turn"
                        );
                        shutdown_after_turn = true;
                        // If the turn has *already* completed by the time
                        // this message lands (e.g. the agent finished and
                        // we're idling between turns), break right now —
                        // there's no `result` event coming to trigger the
                        // post-result check below.
                        if !state.turn_active.load(Ordering::Acquire) {
                            tracing::info!(
                                session_id = %session_id,
                                "No active turn at shutdown request; breaking immediately"
                            );
                            break;
                        }
                        continue;
                    }
                    None => {
                        // All sender handles dropped — the provider has
                        // released the run. We can't accept more turns,
                        // but the child may still be mid-turn; let
                        // stdout drain to its `result` then exit.
                        // Falling through to the next select round is
                        // fine because `recv()` will keep returning
                        // None and the other branches still drive
                        // progress.
                        continue;
                    }
                }
            }
            event = broadcast_rx.recv() => {
                if let Ok(ws_event) = event {
                    if ws_event.session_id == session_id
                        && ws_event.event_type == "worker-stdin-deliver"
                    {
                        if let Some(text) = ws_event.data.get("text").and_then(|v| v.as_str()) {
                            tracing::info!(
                                session_id = %session_id,
                                "Delivering inter-worker message to running agent"
                            );
                            // Inter-worker deliveries are user messages,
                            // not raw control frames, so they need the
                            // stream-json envelope. Inter-worker comms
                            // are text-only (no attachment plumbing on
                            // that path), so we build a plain
                            // `UserMessage::from_text`.
                            let frame = super::build_user_message_frame(&UserMessage::from_text(
                                text,
                            ));
                            if write_stdin_line(&mut stdin_pipe, &frame, &session_id).await {
                                state.turn_active.store(true, Ordering::Release);
                                state.last_activity.store(now_ms(), Ordering::Release);
                            }
                        }
                    }
                }
                continue;
            }
        };

        let EventSource::Stdout(line) = event_source;
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        state.last_activity.store(now_ms(), Ordering::Release);

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

        // `result` is the per-turn completion marker in stream-json
        // mode. We surface it as a peckboard `Completed` event AND
        // reset per-turn state so the next user message produces a
        // fresh agent-start. The child keeps running.
        let is_result_event = json.get("type").and_then(|v| v.as_str()) == Some("result");

        usage_tracker.observe_line(&json);

        let events = parse_stream_json(
            &json,
            &mut conversation_id,
            &mut model_name,
            &mut current_tool_id,
            &mut emitted_start,
        );

        let mut todo_events: Vec<ProviderEvent> = Vec::new();

        for event in &events {
            match event {
                ProviderEvent::ToolStart {
                    tool_use_id,
                    name,
                    input,
                } => {
                    if let Some(snapshot) = task_tracker.on_tool_start(tool_use_id, name, input) {
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
                    // The structured result (where TaskCreate's assigned id
                    // lives) is a sibling of `message` on the raw line, not
                    // part of the tool_result block the parser consumes.
                    if let Some(snapshot) = task_tracker.on_tool_end(
                        tool_use_id,
                        error.is_some(),
                        json.get("tool_use_result"),
                    ) {
                        todo_events.push(ProviderEvent::Todo {
                            todos: snapshot.todos,
                        });
                    }
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
                ProviderEvent::Text { .. } => {
                    if !pending_file_changes.is_empty() {
                        flush_pending_file_changes(
                            &db,
                            &broadcaster,
                            &session_id,
                            std::mem::take(&mut pending_file_changes),
                        )
                        .await;
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

        // After persisting the result-derived events, emit our own
        // Completed (so the UI gets a normal agent-end) and reset
        // per-turn state. The CLI's `result` carries the
        // conversation_id; the parser already captured it into
        // `conversation_id` for us.
        if is_result_event {
            last_result_error = result_error(&json);
            if !pending_file_changes.is_empty() {
                flush_pending_file_changes(
                    &db,
                    &broadcaster,
                    &session_id,
                    std::mem::take(&mut pending_file_changes),
                )
                .await;
            }
            // Settle the turn's token usage from the `result` event and
            // emit `Usage` events BEFORE `Completed` — so a usage_events
            // row joined to this turn's agent-end finds its sibling
            // already persisted. The tracker prefers the per-model
            // `modelUsage` deltas (which include subagent and utility-call
            // tokens the main-loop `usage` object misses).
            let turn_usages = usage_tracker.on_result(&json, model_name.as_deref());
            let turn_context = turn_usages
                .iter()
                .map(|u| u.context_tokens)
                .max()
                .unwrap_or(0);
            emit_turn_usage(&db, &broadcaster, &session_id, turn_usages).await;
            emit_event(
                &db,
                &broadcaster,
                &session_id,
                ProviderEvent::Completed {
                    conversation_id: conversation_id.clone(),
                },
            )
            .await;
            state.turn_active.store(false, Ordering::Release);
            emitted_start = false;
            // Closing any tools still flagged open at result time
            // — keeps spinner state self-consistent across turn
            // boundaries.
            if !pending_tool_names.is_empty() {
                let orphans: Vec<String> = pending_tool_names.keys().cloned().collect();
                for tool_use_id in orphans {
                    emit_event(
                        &db,
                        &broadcaster,
                        &session_id,
                        ProviderEvent::ToolEnd {
                            tool_use_id,
                            output: None,
                            error: Some("tool did not return a result before turn ended".into()),
                            images: Vec::new(),
                        },
                    )
                    .await;
                }
                pending_tool_names.clear();
            }
            saw_clean_completion = true;

            // Recycle-after-turn decisions need the session row (one point
            // lookup per completed turn). A worker whose context crossed the
            // compaction threshold recycles so a ProcessCompletion fires NOW
            // and the completion listener can auto-dispatch a compaction
            // turn; a SUBAGENT always recycles so the listener can report
            // its result to the parent immediately — mid-stream children
            // otherwise only complete on the ~30-minute idle reap.
            // Interactive sessions are never auto-compacted (the UI prompts
            // the user instead) and keep running to --resume normally.
            if !shutdown_after_turn {
                let row = db.get_session(&session_id).await.ok().flatten();
                let is_subagent = row.as_ref().is_some_and(|s| s.parent_session_id.is_some());
                let is_worker = row.is_some_and(|s| s.is_worker);
                if is_subagent {
                    tracing::info!(
                        session_id = %session_id,
                        "Subagent turn complete; recycling child to report the result"
                    );
                    shutdown_after_turn = true;
                } else if is_worker
                    && turn_context >= crate::handover::WORKER_COMPACT_CONTEXT_THRESHOLD
                {
                    tracing::info!(
                        session_id = %session_id,
                        context_tokens = turn_context,
                        "Worker context over compaction threshold; recycling child after this turn"
                    );
                    shutdown_after_turn = true;
                }
            }
            // Graceful-shutdown rendezvous: a tool handler (e.g.
            // `finish_card`) set the flag mid-turn so the response
            // could reach the agent. Now that the turn's `result`
            // has been emitted and we recorded a Completed, drop
            // stdin so the CLI sees EOF and exits naturally. The
            // exit-decision branch below classifies this as turn
            // idle + saw clean completion → no Crashed event.
            if shutdown_after_turn {
                tracing::info!(
                    session_id = %session_id,
                    "Turn complete after shutdown request; exiting stream loop"
                );
                break;
            }
        }
    }

    // Drop stdin pipe to signal EOF to the child process
    drop(stdin_pipe);

    let exit_status = process.child.wait().await;

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
                    images: Vec::new(),
                },
            )
            .await;
        }
    }

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

    // Decide whether to emit a Crashed on process exit:
    //
    //   - graceful shutdown requested (ShutdownAfterTurn) → silent
    //     exit, no Crashed event. Takes priority over the
    //     `turn_was_active` / `!saw_clean_completion` branches so a
    //     handler that fires the signal immediately after spawn (no
    //     turn yet, or mid-turn) still tears down cleanly.
    //   - cancelled mid-turn → emit Crashed{interrupted}
    //   - turn active at exit (no result event reached us) → Crashed
    //   - turn idle, clean stdout EOF after at least one completion
    //     (e.g. the idle reaper or graceful shutdown closed stdin)
    //     → silent exit, no Crashed event
    //   - turn idle, never saw a completion at all (child died
    //     before any user turn) → Crashed{spawn-failed}
    let turn_was_active = state.turn_active.load(Ordering::Acquire);
    state.turn_active.store(false, Ordering::Release);

    // A turn that dies before its `result` (kill, crash, interrupt) never
    // reaches the settle-on-result path above — record whatever the
    // per-message snapshots saw so its tokens aren't lost.
    if turn_was_active {
        let crash_usages = usage_tracker.take_crash_fallback(model_name.as_deref());
        emit_turn_usage(&db, &broadcaster, &session_id, crash_usages).await;
    }

    let is_completed = if shutdown_after_turn {
        // Graceful exit — but only if the final turn's `result` reported
        // success. An error result (expired login, API failure) means the
        // turn did NOT do its work; reporting completed=false routes a
        // handover/compaction doc turn to abort_handover instead of
        // finalize_handover, keeping the conversation intact.
        last_result_error.is_none()
    } else if was_cancelled {
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
    } else if turn_was_active {
        let exit_code = exit_status.ok().and_then(|s| s.code());
        emit_event(
            &db,
            &broadcaster,
            &session_id,
            ProviderEvent::Crashed {
                reason: format!("process exited mid-turn (code {})", exit_code.unwrap_or(-1)),
                exit_code,
                stderr: stderr_text,
            },
        )
        .await;
        false
    } else if !saw_clean_completion {
        let exit_code = exit_status.ok().and_then(|s| s.code());
        emit_event(
            &db,
            &broadcaster,
            &session_id,
            ProviderEvent::Crashed {
                reason: format!(
                    "process exited before producing any output (code {})",
                    exit_code.unwrap_or(-1)
                ),
                exit_code,
                stderr: stderr_text,
            },
        )
        .await;
        false
    } else {
        // Turn idle + saw at least one prior completion → graceful
        // recycle (idle reaper killed it, or the provider was
        // shutting down). Don't emit anything; the UI already saw
        // the last turn's Completed event.
        true
    };

    StreamOutcome {
        completed: is_completed,
        error: last_result_error,
    }
}

/// Outcome of one stream-loop run (one child-process lifetime).
pub struct StreamOutcome {
    /// Ended cleanly: no mid-turn death, no error result on the way out.
    pub completed: bool,
    /// Error text from the last turn's `result` event when it reported a
    /// failure (`is_error: true` or a non-`"success"` subtype) — e.g.
    /// "Failed to authenticate. API Error: 401 …" from an expired login.
    pub error: Option<String>,
}

/// Error text of a `result` event; `None` when it reports success. The CLI
/// flags a failed turn with `is_error: true` and/or a non-"success"
/// subtype (e.g. `error_during_execution`), putting any human-readable
/// message in `result`.
fn result_error(json: &serde_json::Value) -> Option<String> {
    let is_error = json
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let subtype = json.get("subtype").and_then(|v| v.as_str());
    if !is_error && subtype.is_none_or(|s| s == "success") {
        return None;
    }
    Some(
        json.get("result")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .or_else(|| subtype.map(str::to_string))
            .unwrap_or_else(|| "the model reported an error".into()),
    )
}
enum EventSource {
    Stdout(String),
}

/// Emit one `Usage` event per model for a settled turn. Multi-model turns
/// share a single pre-fetched `turn_seq` so the rows roll up as one turn;
/// single-model turns let the DB layer auto-assign it.
async fn emit_turn_usage(
    db: &Db,
    broadcaster: &Arc<Broadcaster>,
    session_id: &str,
    usages: Vec<TurnModelUsage>,
) {
    if usages.is_empty() {
        return;
    }
    let turn_seq = if usages.len() > 1 {
        match db.next_usage_turn_seq(session_id).await {
            Ok(seq) => Some(seq),
            Err(e) => {
                tracing::error!(
                    session_id = session_id,
                    "Failed to fetch shared turn_seq; falling back to per-row seqs: {}",
                    e
                );
                None
            }
        }
    } else {
        None
    };
    for u in usages {
        emit_event(
            db,
            broadcaster,
            session_id,
            ProviderEvent::Usage {
                input_tokens: u.slices.input,
                output_tokens: u.slices.output,
                cache_read_tokens: u.slices.cache_read,
                cache_creation_tokens: u.slices.cache_creation,
                total_tokens: u.slices.total(),
                context_tokens: u.context_tokens,
                model: u.model,
                turn_seq,
            },
        )
        .await;
    }
}

/// Auto-notify worker peers about files this worker just modified.
/// Mirrors the previous inline logic but extracted as a free
/// function so the post-result flush and the per-text flush call
/// the same code.
async fn flush_pending_file_changes(
    db: &Db,
    broadcaster: &Arc<Broadcaster>,
    session_id: &str,
    changes: Vec<String>,
) {
    let Ok(Some(session)) = db.get_session(session_id).await else {
        return;
    };
    if !session.is_worker {
        return;
    }
    let Some(project_id) = session.project_id.as_ref() else {
        return;
    };
    let auto_notify = db
        .get_project(project_id)
        .await
        .ok()
        .flatten()
        .map(|p| p.auto_notify_changes)
        .unwrap_or(true);
    if !auto_notify {
        return;
    }

    let card_title = if let Some(ref card_id) = session.card_id {
        db.get_card(card_id).await.ok().flatten().map(|c| c.title)
    } else {
        None
    };

    let Ok(workers) = db.list_worker_sessions_by_project(project_id).await else {
        return;
    };

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
            if let Ok(Some(c)) = db.get_card(cid).await {
                if c.step == "done" || c.step == "wont_do" {
                    continue;
                }
            }
        }
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

/// Write a line to the child process's stdin pipe. Returns true on
/// success.
async fn write_stdin_line(
    stdin: &mut Option<tokio::process::ChildStdin>,
    text: &str,
    session_id: &str,
) -> bool {
    let Some(pipe) = stdin.as_mut() else {
        tracing::warn!(session_id = %session_id, "No stdin pipe available");
        return false;
    };
    if let Err(e) = pipe.write_all(text.as_bytes()).await {
        tracing::warn!(session_id = %session_id, "Failed to write to stdin: {e}");
        return false;
    }
    if let Err(e) = pipe.write_all(b"\n").await {
        tracing::warn!(session_id = %session_id, "Failed to write newline to stdin: {e}");
        return false;
    }
    if let Err(e) = pipe.flush().await {
        tracing::warn!(session_id = %session_id, "Failed to flush stdin: {e}");
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    //! Integration-style tests for `stream_events`. We back the child
    //! process with `cat` (echoes every line sent on stdin straight to
    //! stdout) so we can synthesise the CLI's exact stream-json output
    //! without depending on the real `claude` binary. Two scenarios
    //! cover the load-bearing graceful-shutdown contract: (1)
    //! `ShutdownAfterTurn` while a turn is in flight defers until the
    //! next `result` event, and (2) the final exit produces no
    //! `Crashed` event in the session log.
    use super::*;
    use crate::db::Db;
    use crate::ws::broadcaster::Broadcaster;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use tokio::process::Command;
    use tokio::time::timeout;

    /// An injected account credential must mask the credential vars the
    /// child would otherwise inherit from peckboard's own environment —
    /// `get_envs` reports a masked var as `(key, None)`.
    #[test]
    fn account_env_masks_inherited_credentials() {
        let mut cmd = Command::new("true");
        let mut env = std::collections::HashMap::new();
        env.insert("CLAUDE_CODE_OAUTH_TOKEN".to_string(), "tok".to_string());
        apply_config_env(&mut cmd, &env);
        let envs: Vec<_> = cmd.as_std().get_envs().collect();
        assert!(envs.contains(&(std::ffi::OsStr::new("ANTHROPIC_API_KEY"), None)));
        assert!(envs.contains(&(
            std::ffi::OsStr::new("CLAUDE_CODE_OAUTH_TOKEN"),
            Some(std::ffi::OsStr::new("tok"))
        )));
    }

    /// No injected credential (the Default account) must leave the
    /// inherited environment untouched.
    #[test]
    fn default_account_env_keeps_inherited_credentials() {
        let mut cmd = Command::new("true");
        apply_config_env(&mut cmd, &std::collections::HashMap::new());
        assert_eq!(cmd.as_std().get_envs().count(), 0);
    }
    /// Drive `cat` as a stand-in CLI. Returns the channels and shared
    /// state the caller needs to interact with the loop, plus a
    /// JoinHandle on the spawned stream_events task. Creates the
    /// folder + session rows so the loop's `emit_event` calls find a
    /// valid foreign-key target — without these, the events table
    /// rejects inserts and the test sees an empty event log.
    async fn spawn_cat_loop(
        session_id: &str,
    ) -> (
        Db,
        mpsc::Sender<StdinMsg>,
        Arc<Notify>,
        Arc<AtomicBool>,
        tokio::task::JoinHandle<StreamOutcome>,
    ) {
        let db = Db::in_memory().unwrap();
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(crate::db::models::NewFolder {
            id: "f1".into(),
            name: "f".into(),
            path: "/tmp".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_session(crate::db::models::NewSession {
            id: session_id.into(),
            name: "test".into(),
            folder_id: "f1".into(),
            model: None,
            effort: None,
            is_worker: false,
            project_id: None,
            card_id: None,
            conversation_id: None,
            created_at: ts.clone(),
            last_activity: ts,
            ..Default::default()
        })
        .await
        .unwrap();

        let broadcaster = Broadcaster::new();
        let (tx, rx) = mpsc::channel::<StdinMsg>(16);
        let cancel = Arc::new(Notify::new());
        let turn_active = Arc::new(AtomicBool::new(false));
        let last_activity = Arc::new(AtomicU64::new(0));
        let state = LoopState {
            turn_active: turn_active.clone(),
            last_activity,
        };

        let process = ClaudeProcess::from_command_for_test(Command::new("cat"), session_id)
            .expect("spawn cat");

        let db_clone = db.clone();
        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(async move {
            stream_events(
                process,
                db_clone,
                broadcaster,
                rx,
                "/tmp".into(),
                cancel_clone,
                state,
            )
            .await
        });

        (db, tx, cancel, turn_active, handle)
    }

    /// Synthetic stream-json `result` frame matching the CLI's shape
    /// closely enough for the loop's per-turn completion path to fire.
    fn fake_result_frame(conv_id: &str) -> String {
        serde_json::json!({
            "type": "result",
            "session_id": conv_id,
            "result": "ok",
        })
        .to_string()
    }

    /// Spin until `turn_active` reaches `expected` or `deadline` runs
    /// out. Polling is preferable to a fixed sleep because the loop
    /// flips this flag on its own schedule (after writing the user
    /// envelope), and a too-short sleep would race; a too-long sleep
    /// would slow the suite unnecessarily.
    async fn wait_for_turn_active(flag: &AtomicBool, expected: bool, deadline_ms: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(deadline_ms);
        while flag.load(Ordering::Acquire) != expected {
            if std::time::Instant::now() >= deadline {
                panic!("turn_active never reached {expected}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn shutdown_after_turn_during_turn_breaks_after_result() {
        // The realistic flow: a tool handler calls shutdown_after_turn
        // while the agent is mid-turn. The loop must finish processing
        // the current turn (here, simulated by a `result` frame echoed
        // back via cat) and then exit silently. The session must show
        // a Completed agent-end, not a Crashed one.
        let session = "loop-shutdown-mid-turn";
        let (db, tx, _cancel, turn_active, handle) = spawn_cat_loop(session).await;

        // Drive turn_active=true the same way production does: send a
        // user envelope. The loop writes it to cat's stdin (cat echoes
        // it back, which the parser ignores as a non-result type) and
        // flips the flag after the write succeeds.
        tx.send(StdinMsg::UserTurn(UserMessage::from_text("hello")))
            .await
            .unwrap();
        wait_for_turn_active(&turn_active, true, 2_000).await;

        // Schedule graceful shutdown. The loop sets a flag and
        // continues — does NOT break yet because the turn is active.
        tx.send(StdinMsg::ShutdownAfterTurn).await.unwrap();

        // Deliver the `result` frame as a raw line. cat echoes it,
        // the loop reads it from "stdout", parses it as a result,
        // emits Completed, then checks the flag and breaks.
        tx.send(StdinMsg::RawLine(fake_result_frame("conv-1")))
            .await
            .unwrap();

        let outcome = timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("stream loop must exit within 5s")
            .expect("task must not panic");
        assert!(
            outcome.completed,
            "graceful shutdown after result must report completed=true"
        );

        let events = db.list_events_by_session(session, None).await.unwrap();
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
        assert!(
            kinds.contains(&"agent-end"),
            "result event must emit a Completed agent-end, got kinds: {kinds:?}"
        );
        // Critical: no Crashed agent-end. The user-visible "worker
        // crash" line in this card's description came from the
        // agent-end whose status is Crashed; with graceful shutdown
        // we get a Completed agent-end instead.
        for e in &events {
            if e.kind == "agent-end" {
                let data: serde_json::Value = serde_json::from_str(&e.data).unwrap_or_default();
                let status = data.get("status").and_then(|v| v.as_str()).unwrap_or("");
                assert_ne!(
                    status, "crashed",
                    "graceful shutdown must not produce a Crashed agent-end, got {data}"
                );
            }
        }
    }

    /// Synthetic error `result` frame — the shape the CLI emits when the
    /// turn itself failed (here, the expired-login 401 that triggered the
    /// compaction-finalized-on-auth-failure regression).
    fn fake_error_result_frame(conv_id: &str) -> String {
        serde_json::json!({
            "type": "result",
            "subtype": "error_during_execution",
            "is_error": true,
            "session_id": conv_id,
            "result": "Failed to authenticate. API Error: 401 Invalid authentication credentials",
        })
        .to_string()
    }

    #[tokio::test]
    async fn error_result_reports_not_completed_on_graceful_shutdown() {
        // The compaction-401 regression: begin_handover dispatches the doc
        // turn and schedules a graceful shutdown; the turn errors (the CLI
        // emits an is_error result) and the child exits. The outcome must
        // be completed=false carrying the CLI's error text, so the
        // completion listener aborts the handover instead of finalizing a
        // "compaction" whose doc is an error message.
        let session = "loop-error-result";
        let (_db, tx, _cancel, turn_active, handle) = spawn_cat_loop(session).await;

        tx.send(StdinMsg::UserTurn(UserMessage::from_text("compact")))
            .await
            .unwrap();
        wait_for_turn_active(&turn_active, true, 2_000).await;

        tx.send(StdinMsg::ShutdownAfterTurn).await.unwrap();
        tx.send(StdinMsg::RawLine(fake_error_result_frame("conv-e1")))
            .await
            .unwrap();

        let outcome = timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("stream loop must exit within 5s")
            .expect("task must not panic");
        assert!(
            !outcome.completed,
            "an error result must not count as a completed run"
        );
        assert!(
            outcome.error.as_deref().unwrap_or("").contains("401"),
            "outcome must carry the CLI's error text, got {:?}",
            outcome.error
        );
    }

    #[tokio::test]
    async fn shutdown_after_turn_with_no_active_turn_exits_immediately() {
        // Edge case: a stray shutdown_after_turn while no turn is in
        // flight (e.g. the agent already finished and we're idling)
        // should break out without waiting for a result event. The
        // hardened exit branch is the load-bearing piece: even though
        // `saw_clean_completion` is false here, we still exit silently
        // because the break was due to the graceful path.
        let session = "loop-shutdown-idle";
        let (db, tx, _cancel, turn_active, handle) = spawn_cat_loop(session).await;
        assert!(!turn_active.load(Ordering::Acquire));

        tx.send(StdinMsg::ShutdownAfterTurn).await.unwrap();

        let outcome = timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("stream loop must exit within 5s")
            .expect("task must not panic");
        assert!(
            outcome.completed,
            "graceful exit must report completed=true"
        );

        // No Crashed event even though we never saw a clean completion.
        let events = db.list_events_by_session(session, None).await.unwrap();
        for e in &events {
            if e.kind == "agent-end" {
                let data: serde_json::Value = serde_json::from_str(&e.data).unwrap_or_default();
                let status = data.get("status").and_then(|v| v.as_str()).unwrap_or("");
                assert_ne!(status, "crashed", "expected silent exit, got {data}");
            }
        }
    }

    #[tokio::test]
    async fn explicit_cancel_still_produces_crashed_event() {
        // Regression guard for the OTHER path: user-initiated cancel
        // (the manual "kill worker" action) must still emit
        // Crashed{interrupted}. The graceful flag short-circuits ONLY
        // for ShutdownAfterTurn; cancel.notify must continue to write
        // an explicit crash event so the UI distinguishes "user killed
        // it" from "the worker finished and exited."
        let session = "loop-explicit-cancel";
        let (db, tx, cancel, turn_active, handle) = spawn_cat_loop(session).await;

        // Park a turn in flight so the cancel path's "killed mid-turn"
        // semantics fire. Using UserTurn avoids the race that would
        // come from flipping the flag directly.
        tx.send(StdinMsg::UserTurn(UserMessage::from_text("hello")))
            .await
            .unwrap();
        wait_for_turn_active(&turn_active, true, 2_000).await;

        cancel.notify_one();

        let outcome = timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("stream loop must exit within 5s")
            .expect("task must not panic");
        assert!(
            !outcome.completed,
            "cancelled run must report completed=false"
        );

        let events = db.list_events_by_session(session, None).await.unwrap();
        let crashed = events.iter().find(|e| {
            if e.kind != "agent-end" {
                return false;
            }
            let data: serde_json::Value = serde_json::from_str(&e.data).unwrap_or_default();
            data.get("status").and_then(|v| v.as_str()) == Some("crashed")
        });
        assert!(crashed.is_some(), "user cancel must emit a Crashed event");
    }

    #[tokio::test]
    async fn task_tool_calls_assemble_into_persisted_todo_snapshots() {
        // Claude Code ≥ 2.1 reports work items via incremental
        // TaskCreate/TaskUpdate calls instead of the replace-all TodoWrite.
        // Feed the CLI's exact line shapes (captured from a real 2.1.153
        // stream-json session) through the loop and assert they assemble
        // into `todo` events mirrored to the session's todos table.
        let session = "loop-task-tools";
        let (db, tx, _cancel, _turn_active, handle) = spawn_cat_loop(session).await;

        let lines = [
            serde_json::json!({
                "type": "assistant",
                "message": { "role": "assistant", "content": [
                    { "type": "tool_use", "id": "toolu_1", "name": "TaskCreate",
                      "input": { "subject": "Write code", "description": "details",
                                 "activeForm": "Writing code" } }
                ]},
            }),
            serde_json::json!({
                "type": "user",
                "message": { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_1",
                      "content": "Task #1 created successfully: Write code" }
                ]},
                "tool_use_result": { "task": { "id": "1", "subject": "Write code" } },
            }),
            serde_json::json!({
                "type": "assistant",
                "message": { "role": "assistant", "content": [
                    { "type": "tool_use", "id": "toolu_2", "name": "TaskUpdate",
                      "input": { "taskId": "1", "status": "in_progress" } }
                ]},
            }),
            serde_json::json!({
                "type": "user",
                "message": { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_2",
                      "content": "Updated task #1 status" }
                ]},
                "tool_use_result": { "success": true, "taskId": "1",
                    "updatedFields": ["status"],
                    "statusChange": { "from": "pending", "to": "in_progress" } },
            }),
        ];
        for line in lines {
            tx.send(StdinMsg::RawLine(line.to_string())).await.unwrap();
        }

        // Poll until both snapshots landed (cat's echo is asynchronous).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let todos = db.list_session_todos(session).await.unwrap();
            if todos.len() == 1 && todos[0].status == crate::todo::TodoStatus::InProgress {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("task tools never assembled into todos, got {todos:?}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let todos = db.list_session_todos(session).await.unwrap();
        assert_eq!(todos[0].content, "Write code");
        assert_eq!(todos[0].active_form.as_deref(), Some("Writing code"));

        // One `todo` event per effective change: create, then status update.
        let events = db.list_events_by_session(session, None).await.unwrap();
        let todo_count = events.iter().filter(|e| e.kind == "todo").count();
        assert_eq!(todo_count, 2, "expected create + update snapshots");

        tx.send(StdinMsg::ShutdownAfterTurn).await.unwrap();
        let _ = timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("stream loop must exit within 5s");
    }
}
