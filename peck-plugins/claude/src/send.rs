//! Drive one Claude CLI turn through host spawn / read_line / stdin.

use serde_json::{Value, json};

use crate::argv::{self, CliSpec};
use crate::background::BackgroundTracker;
use crate::event::{CrashKind, ProviderEvent};
use crate::host::{self, HostFn};
use crate::parser::{self, ParserState};
use crate::sandbox;
use crate::todo::TaskTracker;
use crate::usage::UsageTracker;

pub fn run(payload: &Value) -> Result<(), String> {
    let session_id = payload
        .get("session_id")
        .and_then(|v| v.as_str())
        .ok_or("provider.send payload missing session_id")?;
    let cfg = payload.get("spawn_config").cloned().unwrap_or(json!({}));
    let message = payload.get("message").cloned().unwrap_or(json!({}));
    let conversation_id = payload
        .get("conversation_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let system_prompt = payload
        .get("system_prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let working_dir = cfg
        .get("working_dir")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if working_dir.is_empty() {
        return Err("spawn_config.working_dir is empty".into());
    }

    let account = host::call_host(
        HostFn::ProviderAccountEnv,
        &json!({ "session_id": session_id, "model": cfg.get("model") }),
    )?;
    let mut env = map_string_map(cfg.get("env"));
    if let Some(extra) = account.get("env").and_then(|v| v.as_object()) {
        for (k, v) in extra {
            if let Some(s) = v.as_str() {
                env.insert(k.clone(), s.to_string());
            }
        }
    }
    let env_remove: Vec<String> = account
        .get("env_remove")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let mcp = host::call_host(
        HostFn::ProviderGetMcpConfig,
        &json!({ "session_id": session_id }),
    )
    .unwrap_or(json!({}));
    let mcp_path = mcp
        .get("path")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            cfg.get("mcp_config_path")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });
    let core_tools = string_list(mcp.get("core_tools"));
    let pre_hatcher_tools = string_list(mcp.get("pre_hatcher_tools"));

    let _ = host::call_host(
        HostFn::ProviderWriteFile,
        &json!({
            "session_id": session_id,
            "path": argv::HOOK_CONTEXT_FILE,
            "contents": argv::subagent_context_json(),
        }),
    );
    let subagent_path = format!("{working_dir}/{}", argv::HOOK_CONTEXT_FILE);

    let spec = CliSpec {
        model: cfg
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string(),
        effort: cfg
            .get("effort")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        conversation_id: conversation_id.clone(),
        mcp_config_path: mcp_path,
        permission_mode: cfg
            .get("permission_mode")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        is_worker: cfg
            .get("is_worker")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        is_pre_hatcher: cfg
            .get("is_pre_hatcher")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        extra_allowed_tools: string_list(cfg.get("extra_allowed_tools")),
        extra_disallowed_tools: string_list(cfg.get("extra_disallowed_tools")),
        system_prompt,
        core_tools,
        pre_hatcher_tools,
        subagent_context_path: Some(subagent_path),
    };
    let args = argv::build_cli_args(&spec);

    host::call_host(
        HostFn::ProviderSpawn,
        &json!({
            "session_id": session_id,
            "command": crate::settings::cli_path("claude"),
            "args": args,
            "env": env,
            "env_remove": env_remove,
            "cwd": working_dir,
        }),
    )?;

    let frame = argv::build_user_message_frame(&message);
    write_stdin(session_id, &frame)?;

    let mut parser = ParserState::new();
    let mut usage = UsageTracker::default();
    let mut tasks = TaskTracker::new();
    let mut background = BackgroundTracker::new();
    let mut winding_down = false;
    let mut last_result_error: Option<String> = None;
    // True once this send has settled a genuine turn `result`; from then
    // on the loop is only lingering for in-flight background subagents
    // (see `background.rs`) and every exit is a clean completion.
    let mut turn_settled = false;
    // Whether the host has been told this settled turn is lingering (see
    // `announce_linger`); reset when an injected message starts a new turn.
    let mut linger_announced = false;
    // Armed only while lingering, so a hung background agent cannot pin
    // the CLI child (and the session's turn slot) forever.
    let mut linger_guard = Watchdog::arm(None);
    // Per-turn watchdog from `SpawnConfig.timeout_ms`, re-armed for every
    // injected follow-up turn — a wedged turn dies instead of running
    // until a manual cancel.
    let timeout_ms = cfg.get("timeout_ms").and_then(|v| v.as_u64());
    let mut watchdog = Watchdog::arm(timeout_ms);

    loop {
        // A set stop flag is a HARD stop: on interrupt the host writes the
        // CLI's in-band interrupt frame itself and only sets the flag (and
        // reaps the child) after the grace window — or immediately on
        // cancel. Nothing to send from here; just record the wind-down so
        // the eof path reports `interrupted`.
        if !winding_down && should_stop(session_id) {
            winding_down = true;
        }
        // A stop after the turn settled only ends the background linger:
        // exit clean (the turn IS complete), never as an interrupt crash.
        if winding_down && turn_settled {
            return settle_and_exit(session_id, &background, "stop requested");
        }

        // The per-turn watchdog bounds a running turn only; once it settled,
        // the linger cap alone decides how long background work may run.
        if (!turn_settled && watchdog.expired()) || linger_guard.expired() {
            if turn_settled {
                return settle_and_exit(session_id, &background, "linger timeout");
            }
            let _ = kill(session_id);
            return finish_timeout(session_id, &mut parser, &mut usage, timeout_ms);
        }

        if let Some(msg) = take_message(session_id)? {
            let frame = argv::build_user_message_frame(&msg);
            let _ = write_stdin(session_id, &frame);
            last_result_error = None;
            parser.reset_turn();
            watchdog.rearm();
            turn_settled = false;
            linger_guard = Watchdog::arm(None);
            linger_announced = false;
        }

        let line = read_line(session_id, 100)?;
        if line
            .get("stopped")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            // Stop raced the host's child teardown; make sure the child is
            // gone and wind down — the next read returns eof.
            winding_down = true;
            let _ = kill(session_id);
            continue;
        }
        if line
            .get("timeout")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        if line.get("eof").and_then(|v| v.as_bool()).unwrap_or(false) {
            let exit_code = line
                .get("exit_code")
                .and_then(|v| v.as_i64())
                .map(|n| n as i32);
            let stderr = line
                .get("stderr")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let signal = line
                .get("signal")
                .and_then(|v| v.as_i64())
                .map(|n| n as i32);
            if turn_settled {
                // The CLI exited on its own after the turn settled (e.g.
                // done with its background work); nothing crashed.
                return settle_and_exit(session_id, &background, "claude exited");
            }
            finish(
                session_id,
                &mut parser,
                &mut usage,
                last_result_error,
                winding_down,
                signal,
                exit_code,
                stderr,
            )?;
            return Ok(());
        }
        let Some(raw) = line.get("line").and_then(|v| v.as_str()) else {
            continue;
        };
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let json_line: Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if json_line.get("type").and_then(|v| v.as_str()) == Some("control_response") {
            // Interrupt acks included: the host owns the soft-interrupt
            // frame now, and either the CLI settles the turn with a real
            // `result` or the host hard-kills after the grace window.
            continue;
        }

        if json_line.get("type").and_then(|v| v.as_str()) == Some("control_request") {
            handle_control(session_id, &json_line, &working_dir)?;
            continue;
        }

        usage.observe_line(&json_line);
        let is_result = json_line.get("type").and_then(|v| v.as_str()) == Some("result")
            && (is_turn_result(&json_line) || (turn_settled && is_notification_result(&json_line)));
        let events = parser::parse_stream_json(&json_line, &mut parser);
        // Background-subagent bookkeeping: a task-notification user frame
        // settles its pending id (the frame itself carries no tool blocks,
        // so the parser emits nothing for it).
        background.on_stream_line(&json_line);
        // Assemble TodoWrite / TaskCreate / TaskUpdate calls into replace-
        // all `todo` snapshots as the tools stream (0.1.11's task_tracker).
        let mut todo_events: Vec<ProviderEvent> = Vec::new();
        for ev in &events {
            match ev {
                ProviderEvent::ToolStart {
                    tool_use_id,
                    name,
                    input,
                } => {
                    background.on_tool_start(tool_use_id, name, input);
                    if let Some(todos) = tasks.on_tool_start(tool_use_id, name, input) {
                        todo_events.push(ProviderEvent::Todo { todos });
                    }
                }
                ProviderEvent::ToolEnd {
                    tool_use_id,
                    output,
                    error,
                    ..
                } => {
                    // The structured result (where TaskCreate's assigned id
                    // lives) is a sibling of `message` on the raw line, not
                    // part of the tool_result block the parser consumes.
                    background.on_tool_end(
                        tool_use_id,
                        error.is_some(),
                        json_line.get("tool_use_result"),
                        output.as_deref(),
                    );
                    if let Some(todos) = tasks.on_tool_end(
                        tool_use_id,
                        error.is_some(),
                        json_line.get("tool_use_result"),
                    ) {
                        todo_events.push(ProviderEvent::Todo { todos });
                    }
                }
                _ => {}
            }
        }
        for ev in events {
            emit_live(session_id, &ev, turn_settled)?;
        }
        for ev in todo_events {
            emit_live(session_id, &ev, turn_settled)?;
        }
        if is_result {
            last_result_error = result_error(&json_line);
            let usages = usage.on_result(&json_line, parser.model_name.as_deref());
            for u in usages {
                emit_live(
                    session_id,
                    &ProviderEvent::Usage {
                        input_tokens: u.slices.input,
                        output_tokens: u.slices.output,
                        cache_read_tokens: u.slices.cache_read,
                        cache_creation_tokens: u.slices.cache_creation,
                        total_tokens: u.slices.total(),
                        context_tokens: u.context_tokens,
                        model: u.model,
                        turn_seq: None,
                    },
                    turn_settled,
                )?;
            }
            let mut meta = result_meta(&json_line);
            if let Some(err) = last_result_error.as_ref() {
                if meta.is_null() {
                    meta = json!({});
                }
                if let Some(obj) = meta.as_object_mut() {
                    obj.insert("error".into(), json!(err));
                    // camelCase on the wire: the web reader and the host's
                    // terminal_from_completed both key on `errorKind`, and
                    // every error result gets the full classification (not
                    // just auth) so resume/rate-limit recovery can fire.
                    obj.insert("errorKind".into(), json!(CrashKind::classify(err).as_str()));
                }
            }
            emit_live(
                session_id,
                &ProviderEvent::Completed {
                    conversation_id: parser.conversation_id.clone(),
                    result_meta: meta,
                },
                turn_settled,
            )?;
            parser.reset_turn();
            turn_settled = true;
            // Drain the injection queue once more before tearing down: a
            // message queued between the last poll and this `result` would
            // otherwise die with the child. Deliver it as the next turn of
            // the same child instead.
            if !winding_down && let Some(msg) = take_message(session_id)? {
                let frame = argv::build_user_message_frame(&msg);
                let _ = write_stdin(session_id, &frame);
                last_result_error = None;
                watchdog.rearm();
                turn_settled = false;
                linger_guard = Watchdog::arm(None);
                linger_announced = false;
                continue;
            }
            // Background subagents still running inside the CLI: linger —
            // killing now would orphan them (0.1.15 and earlier lost the
            // work; --resume only replayed a "stopped" notification).
            // Their task-notification turns stream through this same loop
            // and settle above.
            if !winding_down && background.pending() > 0 {
                linger_guard = Watchdog::arm(Some(BACKGROUND_LINGER_CAP_MS));
                if !linger_announced {
                    announce_linger(session_id, &background);
                    linger_announced = true;
                }
                continue;
            }
            // One provider.send = one turn. The CLI child is dropped when
            // we return; next send respawns (or resumes via --resume).
            return settle_and_exit(session_id, &background, "stop requested");
        }
    }
}

/// Cap on how long a settled turn lingers for in-flight background
/// subagents before giving up and tearing the CLI child down anyway.
const BACKGROUND_LINGER_CAP_MS: u64 = 60 * 60 * 1000;

/// A `result` frame the CLI stamps for a background task-notification
/// turn (`origin: {"kind": "task-notification"}`). While lingering these
/// settle the notification's follow-up turn; at turn start they are
/// resume replays and stay skipped (see `is_turn_result`).
fn is_notification_result(json: &Value) -> bool {
    json.get("origin")
        .and_then(|o| o.get("kind"))
        .and_then(|k| k.as_str())
        == Some("task-notification")
}

/// `System` subtype that tells the host this settled turn is lingering for
/// background subagents. Must match the host's
/// `plugin_provider::BACKGROUND_LINGER_SUBTYPE`: the host then treats the
/// session as idle and injects new messages into this child as its next
/// turn instead of queueing them until the linger ends.
const BACKGROUND_LINGER_SUBTYPE: &str = "background-linger";

/// Tell the host (and the user) the turn settled but the CLI stays up for
/// in-flight background subagents.
fn announce_linger(session_id: &str, background: &BackgroundTracker) {
    let ids = background.pending_ids();
    let _ = emit(
        session_id,
        &ProviderEvent::System {
            text: format!(
                "{n} background task(s) still running ({list}); keeping the agent \
                 process alive until they finish. New messages go straight to it.",
                n = ids.len(),
                list = ids.join(", "),
            ),
            subtype: BACKGROUND_LINGER_SUBTYPE.into(),
            detail: json!({ "ids": ids }),
        },
    );
}

/// [`emit`], except that once the turn has settled a refused emit does not
/// abort the send: an error return ends the run, and the host then kills
/// the child — with every background subagent still running in it.
fn emit_live(session_id: &str, event: &ProviderEvent, turn_settled: bool) -> Result<(), String> {
    match emit(session_id, event) {
        Err(_) if turn_settled => Ok(()),
        other => other,
    }
}

/// Tear down after the turn has settled (possibly mid-linger). If
/// background subagents are still in flight, leave a visible note first —
/// the next turn's `--resume` replays their notifications as "stopped".
fn settle_and_exit(
    session_id: &str,
    background: &BackgroundTracker,
    why: &str,
) -> Result<(), String> {
    if background.pending() > 0 {
        let ids = background.pending_ids();
        let _ = emit(
            session_id,
            &ProviderEvent::System {
                text: format!(
                    "{n} background task(s) still running ({list}) were terminated ({why}); \
                     they will be reported as stopped next turn",
                    n = background.pending(),
                    list = ids.join(", "),
                ),
                subtype: "background-subagents".into(),
                detail: json!({ "ids": ids, "reason": why }),
            },
        );
    }
    let _ = kill(session_id);
    Ok(())
}
fn finish(
    session_id: &str,
    parser: &mut ParserState,
    usage: &mut UsageTracker,
    last_result_error: Option<String>,
    interrupted: bool,
    signal: Option<i32>,
    exit_code: Option<i32>,
    stderr: Option<String>,
) -> Result<(), String> {
    for u in usage.take_crash_fallback(parser.model_name.as_deref()) {
        emit(
            session_id,
            &ProviderEvent::Usage {
                input_tokens: u.slices.input,
                output_tokens: u.slices.output,
                cache_read_tokens: u.slices.cache_read,
                cache_creation_tokens: u.slices.cache_creation,
                total_tokens: u.slices.total(),
                context_tokens: u.context_tokens,
                model: u.model,
                turn_seq: None,
            },
        )?;
    }
    let (reason, kind) = classify_exit(last_result_error, interrupted, signal, stderr.as_deref());
    emit(
        session_id,
        &ProviderEvent::Crashed {
            reason,
            error_kind: kind,
            exit_code,
            stderr,
        },
    )
}

/// Reason + kind for a run that ended without a settling `result`.
///
/// Classification order matches 0.1.11's process loop: the CLI's own error
/// text wins; otherwise the stderr tail often names the real cause (a
/// resume rejection, an expired login, a 429) even when the *shape* of the
/// exit only says "no output" — so it is classified first and `NoOutput`
/// is only the fallback when stderr carries nothing recognizable.
/// A signal kill outranks the stderr sniff: the OS took the child down
/// (OOM killer, service shutdown), so any stderr tail predates the kill.
fn classify_exit(
    last_result_error: Option<String>,
    interrupted: bool,
    signal: Option<i32>,
    stderr: Option<&str>,
) -> (String, CrashKind) {
    if interrupted {
        return ("interrupted".into(), CrashKind::Interrupted);
    }
    if let Some(err) = last_result_error {
        let kind = CrashKind::classify(&err);
        return (err, kind);
    }
    if let Some(sig) = signal {
        return (
            killed_by_signal_reason("claude", sig),
            CrashKind::ExitedMidTurn,
        );
    }
    let stderr_text = stderr.map(str::to_string).filter(|s| !s.trim().is_empty());
    let kind = CrashKind::classify_or(stderr_text.as_deref().unwrap_or(""), CrashKind::NoOutput);
    (
        stderr_text.unwrap_or_else(|| "claude exited without a result".into()),
        kind,
    )
}

/// Reason for a child that died to a signal instead of exiting: the OS or
/// service manager killed it mid-turn — the CLI itself never got to report
/// anything, so "exited without a result" would point at the wrong culprit.
fn killed_by_signal_reason(provider: &str, signal: i32) -> String {
    let name = match signal {
        6 => " (SIGABRT)",
        9 => " (SIGKILL)",
        11 => " (SIGSEGV)",
        15 => " (SIGTERM)",
        _ => "",
    };
    format!(
        "{provider} was killed by signal {signal}{name} before finishing — \
         the OS or service manager took it down mid-turn (usually the \
         out-of-memory killer; check `journalctl -k` for oom-kill events). \
         Send the message again to retry."
    )
}

/// The turn watchdog tripped (`SpawnConfig.timeout_ms`): settle the turn's
/// tokens from the per-message snapshots, then report the same crash
/// 0.1.11's process loop emitted.
fn finish_timeout(
    session_id: &str,
    parser: &mut ParserState,
    usage: &mut UsageTracker,
    timeout_ms: Option<u64>,
) -> Result<(), String> {
    for u in usage.take_crash_fallback(parser.model_name.as_deref()) {
        emit(
            session_id,
            &ProviderEvent::Usage {
                input_tokens: u.slices.input,
                output_tokens: u.slices.output,
                cache_read_tokens: u.slices.cache_read,
                cache_creation_tokens: u.slices.cache_creation,
                total_tokens: u.slices.total(),
                context_tokens: u.context_tokens,
                model: u.model,
                turn_seq: None,
            },
        )?;
    }
    emit(
        session_id,
        &ProviderEvent::Crashed {
            reason: format!("turn timeout after {}ms", timeout_ms.unwrap_or_default()),
            error_kind: CrashKind::Timeout,
            exit_code: None,
            stderr: None,
        },
    )
}

/// Wall-clock guard for one turn, armed from `SpawnConfig.timeout_ms`.
/// Native builds only — wasm32-unknown-unknown has no monotonic clock, so
/// there the watchdog never fires and only host-side bounds apply.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
struct Watchdog {
    timeout_ms: Option<u64>,
    #[cfg(not(target_arch = "wasm32"))]
    deadline: Option<std::time::Instant>,
}

impl Watchdog {
    fn arm(timeout_ms: Option<u64>) -> Self {
        let mut w = Watchdog {
            timeout_ms,
            #[cfg(not(target_arch = "wasm32"))]
            deadline: None,
        };
        w.rearm();
        w
    }

    fn rearm(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.deadline = self
                .timeout_ms
                .map(|ms| std::time::Instant::now() + std::time::Duration::from_millis(ms));
        }
    }

    fn expired(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.deadline
                .is_some_and(|d| std::time::Instant::now() >= d)
        }
        #[cfg(target_arch = "wasm32")]
        {
            false
        }
    }
}

fn handle_control(session_id: &str, json: &Value, allowed_dir: &str) -> Result<(), String> {
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
    if subtype != "can_use_tool" {
        return Ok(());
    }
    if tool_name == "AskUserQuestion" {
        let input = request.and_then(|r| r.get("input"));
        let questions = parser::normalize_questions(input);
        let tool_use_id = request
            .and_then(|r| r.get("tool_use_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        emit(
            session_id,
            &ProviderEvent::ControlRequest {
                request_id: request_id.to_string(),
                request_type: "AskUserQuestion".into(),
                payload: json!({
                    "toolUseId": tool_use_id,
                    "questions": questions,
                }),
            },
        )?;
        return Ok(());
    }
    let input = request
        .and_then(|r| r.get("input"))
        .cloned()
        .unwrap_or(json!({}));
    let denied = sandbox::check_path_violation(tool_name, &input, allowed_dir);
    let response = if let Some(reason) = denied {
        json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": { "behavior": "deny", "message": reason },
            }
        })
    } else {
        json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": { "behavior": "allow", "updatedInput": input },
            }
        })
    };
    write_stdin(session_id, &response.to_string())?;
    Ok(())
}

fn emit(session_id: &str, event: &ProviderEvent) -> Result<(), String> {
    let value = serde_json::to_value(event).map_err(|e| e.to_string())?;
    host::call_host(
        HostFn::EmitProviderEvent,
        &json!({ "session_id": session_id, "event": value }),
    )
    .map(|_| ())
}

fn write_stdin(session_id: &str, text: &str) -> Result<(), String> {
    let mut line = text.to_string();
    if !line.ends_with('\n') {
        line.push('\n');
    }
    host::call_host(
        HostFn::ProviderWriteStdin,
        &json!({ "session_id": session_id, "text": line }),
    )
    .map(|_| ())
}

fn read_line(session_id: &str, timeout_ms: u64) -> Result<Value, String> {
    host::call_host(
        HostFn::ProviderReadLine,
        &json!({ "session_id": session_id, "timeout_ms": timeout_ms }),
    )
}

fn kill(session_id: &str) -> Result<(), String> {
    host::call_host(HostFn::ProviderKill, &json!({ "session_id": session_id })).map(|_| ())
}

fn should_stop(session_id: &str) -> bool {
    host::call_host(
        HostFn::ProviderShouldStop,
        &json!({ "session_id": session_id }),
    )
    .ok()
    .and_then(|v| v.get("stop").and_then(|s| s.as_bool()))
    .unwrap_or(false)
}

fn take_message(session_id: &str) -> Result<Option<Value>, String> {
    let v = host::call_host(
        HostFn::ProviderTakeMessage,
        &json!({ "session_id": session_id }),
    )?;
    Ok(v.get("message").cloned().filter(|m| !m.is_null()))
}

/// A `result` frame that settles OUR turn — not one the CLI flushed for
/// something it replayed on `--resume`. Claude Code ≥ 2.1 re-delivers
/// orphaned background-task notifications when resuming a conversation and
/// emits a standalone result frame for each, stamped
/// `origin: {"kind": "task-notification"}` with `num_turns: 0`, an empty
/// `result` and `is_error: false`. Treating the first such frame as the
/// turn terminal completed the turn in ~10 ms as a clean empty success,
/// killed the child before it could mark the notification consumed, and
/// wedged every later turn of the conversation into the same silent empty
/// completion (the 0.1.13 "agent replies with nothing, no error" bug). A
/// user-turn result carries no `origin`; skipping a foreign frame lets the
/// loop keep reading until the real turn settles (worst case: eof →
/// `classify_exit`, a visible crash instead of a silent success).
fn is_turn_result(json: &Value) -> bool {
    if json.get("origin").is_some() {
        return false;
    }
    let is_error = json
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if is_error {
        return true;
    }
    // Belt and braces for origin-less variants: a clean result that counts
    // zero turns and carries no text cannot be the answer to the prompt we
    // wrote — the CLI never processed it.
    let zero_turns = json.get("num_turns").and_then(|v| v.as_i64()) == Some(0);
    let empty = json
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .is_empty();
    !(zero_turns && empty)
}

/// Error text of a failed `result` frame. The CLI puts a model-side failure
/// in `result`, but a startup failure (e.g. `--resume` of a transcript it
/// can't load) arrives as `subtype: error_during_execution` with an empty
/// `result` and the real reason in `errors[]` — without reading that array
/// the text was just the subtype, which classifies as `unknown` and kept
/// resume recovery from ever firing.
fn result_error(json: &Value) -> Option<String> {
    let is_error = json
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let subtype = json.get("subtype").and_then(|v| v.as_str());
    if !is_error && subtype.is_none_or(|s| s == "success") {
        return None;
    }
    let errors = string_list(json.get("errors"))
        .into_iter()
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    Some(
        json.get("result")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .or_else(|| (!errors.is_empty()).then_some(errors))
            .or_else(|| subtype.map(str::to_string))
            .unwrap_or_else(|| "the model reported an error".into()),
    )
}

fn result_meta(json: &Value) -> Value {
    let mut meta = serde_json::Map::new();
    if let Some(v) = json.get("duration_ms").and_then(|v| v.as_i64()) {
        meta.insert("durationMs".into(), v.into());
    }
    if let Some(v) = json.get("num_turns").and_then(|v| v.as_i64()) {
        meta.insert("numTurns".into(), v.into());
    }
    if let Some(n) = json
        .get("total_cost_usd")
        .and_then(|v| v.as_f64())
        .and_then(serde_json::Number::from_f64)
    {
        meta.insert("totalCostUsd".into(), Value::Number(n));
    }
    if let Some(denials) = json
        .get("permission_denials")
        .and_then(|v| v.as_array())
        .filter(|d| !d.is_empty())
    {
        meta.insert("permissionDenials".into(), Value::Array(denials.clone()));
    }
    if meta.is_empty() {
        Value::Null
    } else {
        Value::Object(meta)
    }
}

fn string_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn map_string_map(v: Option<&Value>) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    if let Some(obj) = v.and_then(|v| v.as_object()) {
        for (k, val) in obj {
            if let Some(s) = val.as_str() {
                out.insert(k.clone(), s.to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_exit_sorts_stderr_before_no_output() {
        // Resume rejection: wording that the auth bucket must not swallow.
        let (reason, kind) = classify_exit(
            None,
            false,
            None,
            Some("Error: No conversation found with session ID: abc123"),
        );
        assert_eq!(kind, CrashKind::ResumeFailed);
        assert!(reason.contains("No conversation found"));

        // Auth failure from stderr.
        let (_, kind) = classify_exit(
            None,
            false,
            None,
            Some("Failed to authenticate. API Error: 401 Unauthorized"),
        );
        assert_eq!(kind, CrashKind::AuthExpired);

        // Rate limit.
        let (_, kind) = classify_exit(None, false, None, Some("API Error: 429 Too Many Requests"));
        assert_eq!(kind, CrashKind::RateLimit);

        // Nothing usable on stderr → the structural fallback.
        let (reason, kind) = classify_exit(None, false, None, Some("   "));
        assert_eq!(kind, CrashKind::NoOutput);
        assert_eq!(reason, "claude exited without a result");

        // The CLI's own result error wins over stderr and gets the full
        // classification.
        let (reason, kind) = classify_exit(
            Some("No conversation found with session ID: abc123".into()),
            false,
            None,
            Some("unrelated stderr"),
        );
        assert_eq!(kind, CrashKind::ResumeFailed);
        assert_eq!(reason, "No conversation found with session ID: abc123");

        // Wind-down beats everything.
        let (reason, kind) = classify_exit(Some("401".into()), true, None, None);
        assert_eq!(kind, CrashKind::Interrupted);
        assert_eq!(reason, "interrupted");
    }

    #[test]
    fn signal_kill_outranks_stderr_but_not_the_clis_own_error() {
        let (reason, kind) = classify_exit(None, false, Some(9), Some("partial stderr tail"));
        assert_eq!(kind, CrashKind::ExitedMidTurn);
        assert!(reason.contains("killed by signal 9 (SIGKILL)"), "{reason}");

        // The CLI's own result error still wins — it names the real cause.
        let (reason, kind) = classify_exit(Some("API Error: 429".into()), false, Some(9), None);
        assert_eq!(kind, CrashKind::RateLimit);
        assert_eq!(reason, "API Error: 429");
    }

    #[test]
    fn subagent_hook_context_is_the_cli_hook_output_shape() {
        let v: Value = serde_json::from_str(&argv::subagent_context_json()).unwrap();
        assert_eq!(
            v.pointer("/hookSpecificOutput/hookEventName")
                .and_then(|x| x.as_str()),
            Some("SubagentStart")
        );
        assert!(
            v.pointer("/hookSpecificOutput/additionalContext")
                .and_then(|x| x.as_str())
                .is_some_and(|c| c.contains("Peckboard subagent rules"))
        );
        // The hook config path and the written file must stay consistent;
        // both derive from HOOK_CONTEXT_FILE, which lives under a dotted
        // dir so it stays out of the user's way.
        assert!(argv::HOOK_CONTEXT_FILE.starts_with(".peckboard/"));
    }

    /// Captured live from claude 2.1.226 resuming a conversation whose
    /// previous process died with a background task still running: the
    /// replayed notification's result frame must not settle the turn,
    /// while the genuine frames (success, error, even a zero-turn error)
    /// must.
    #[test]
    fn task_notification_results_do_not_settle_the_turn() {
        let notification: Value = serde_json::json!({
            "type": "result", "subtype": "success", "is_error": false,
            "num_turns": 0, "result": "", "duration_ms": 111,
            "total_cost_usd": 0, "origin": { "kind": "task-notification" },
        });
        assert!(!is_turn_result(&notification));

        // Same shape without the origin stamp — still not our turn.
        let originless: Value = serde_json::json!({
            "type": "result", "subtype": "success", "is_error": false,
            "num_turns": 0, "result": "",
        });
        assert!(!is_turn_result(&originless));

        // A real success settles.
        let success: Value = serde_json::json!({
            "type": "result", "subtype": "success", "is_error": false,
            "num_turns": 1, "result": "OK",
        });
        assert!(is_turn_result(&success));

        // A real error settles even at zero turns — the error text must
        // reach the user, not fall through to eof classification.
        let auth_error: Value = serde_json::json!({
            "type": "result", "subtype": "success", "is_error": true,
            "num_turns": 0, "result": "Not logged in · Please run /login",
        });
        assert!(is_turn_result(&auth_error));
    }

    #[test]
    fn notification_results_settle_only_while_lingering() {
        let stamped: Value = serde_json::json!({
            "type": "result", "subtype": "success", "is_error": false,
            "num_turns": 1, "result": "task done",
            "origin": { "kind": "task-notification" },
        });
        assert!(is_notification_result(&stamped));
        // Still never a genuine turn result — replays at turn start must
        // keep being skipped (the 0.1.13 silent-empty-turn bug).
        assert!(!is_turn_result(&stamped));
        let genuine: Value = serde_json::json!({
            "type": "result", "subtype": "success", "is_error": false,
            "num_turns": 1, "result": "OK",
        });
        assert!(!is_notification_result(&genuine));
    }

    /// Captured live from claude resuming a conversation whose transcript
    /// was truncated to 0 bytes (disk full mid-write): empty `result`, the
    /// reason only in `errors[]`. It must classify as a resume failure so
    /// resume_recovery drops the dead id instead of wedging the session.
    #[test]
    fn resume_failure_reason_is_read_from_errors_array() {
        let frame: Value = serde_json::json!({
            "type": "result", "subtype": "error_during_execution",
            "duration_ms": 0, "is_error": true, "num_turns": 0,
            "session_id": "75d27545-6363-41f4-aefd-d802881b05df",
            "errors": ["No conversation found with session ID: 75d27545-6363-41f4-aefd-d802881b05df"],
        });
        assert!(is_turn_result(&frame));
        let err = result_error(&frame).expect("error frame");
        assert!(err.starts_with("No conversation found"), "{err}");
        assert_eq!(CrashKind::classify(&err), CrashKind::ResumeFailed);

        // No `errors[]` and no `result` still falls back to the subtype.
        let bare: Value = serde_json::json!({
            "type": "result", "subtype": "error_during_execution", "is_error": true,
        });
        assert_eq!(
            result_error(&bare).as_deref(),
            Some("error_during_execution")
        );
    }
}
