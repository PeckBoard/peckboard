//! Shared per-turn CLI harness.
//!
//! Three providers — `cursor`, `grok` and `kimi` — drive their agent by
//! invoking a headless CLI **once per turn** and translating the child's
//! newline-delimited JSON stdout into [`ProviderEvent`]s. That loop (spawn,
//! env forwarding, stderr watcher, cancel / timeout select, terminal
//! `Completed` / `Crashed`) used to be copy-pasted three times and drifted:
//! only two of the three had a turn timeout, only two forwarded
//! `SpawnConfig::env`, and only two tagged their `Started` event with the
//! provider. This module is the single implementation; everything that
//! genuinely differs per CLI is passed in as data on [`TurnSpec`], and the
//! stream parsing is the one behaviour supplied as code, via [`TurnStream`].
//!
//! Alongside it live the other helpers those providers (and `ollama`) each
//! had a private copy of: plugin-settings accessors, `additional_models`
//! merging, CLI path resolution, and the non-Claude system-prompt
//! composition.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Notify;

use crate::provider::agent::emit_event;
use crate::provider::stream::{CrashKind, ModelInfo, ProviderEvent, SpawnConfig};

/// Cap on stderr bytes captured for a crash message.
pub const MAX_STDERR_BYTES: usize = 16 * 1024;

/// Default wall-clock bound on a single turn. A per-turn CLI that wedges
/// (a hung tool call, a backend that never answers) would otherwise pin the
/// session forever, since nothing else in the pipeline reaps it.
pub const DEFAULT_TIMEOUT_SECS: u64 = 600;

/// Grace given to a turn whose card was already transitioned by a terminal
/// MCP tool (`complete_step`, `finish_card`, `wont_do_card`) before the child
/// is wound down. Long enough for the agent to take the in-flight tool
/// response and write a closing line; short enough that the card isn't left
/// waiting on output nobody will read. See [`TurnSpec::retire`].
pub const RETIRE_GRACE_SECS: u64 = 15;

/// A stderr substring the harness reacts to, and the crash message it maps
/// to. Auth failures are the reason this exists: an unauthenticated CLI
/// either blocks on a device prompt (`abort`) or exits non-zero with an
/// opaque message (not `abort`), and in both cases the raw stderr is a poor
/// thing to show the user.
pub struct StderrMarker {
    /// Substring matched against each stderr line.
    pub marker: &'static str,
    /// User-facing crash reason to report instead of the raw stderr.
    pub message: &'static str,
    /// `true`: kill the child the moment the marker appears — the CLI is
    /// waiting for an interactive login that will never come. `false`: only
    /// rewrite the crash reason if the turn also exits non-zero.
    /// Taxonomy slot for the crash this marker describes — most markers
    /// are auth failures ([`CrashKind::AuthExpired`]).
    pub kind: CrashKind,
    pub abort: bool,
}

/// Per-turn parsing state. The harness owns the process; the implementor
/// owns the CLI's stream format.
pub trait TurnStream: Send {
    /// Translate one parsed stdout line into provider events.
    fn on_line(&mut self, json: &serde_json::Value) -> Vec<ProviderEvent>;

    /// The conversation/session id to attach to the terminal `Completed`,
    /// if the stream carried one.
    fn take_conversation_id(&mut self) -> Option<String>;

    /// Whether the parser already emitted its own `Started` (cursor derives
    /// one from the CLI's init frame). Only consulted when
    /// [`TurnSpec::started_up_front`] is false.
    fn emitted_start(&self) -> bool {
        false
    }

    /// A terminal error reported *inside* the stream rather than by the exit
    /// status (grok emits an error frame and then still ends cleanly).
    fn take_error(&mut self) -> Option<String> {
        None
    }
}

/// Everything the harness needs for one turn that isn't the stream parser.
pub struct TurnSpec<'a> {
    /// Provider id — used for the `Started` metadata tag, log lines and
    /// timeout/crash copy.
    pub provider: &'static str,
    pub cli_path: &'a str,
    pub args: &'a [String],
    /// Environment forwarded to the child, on top of the server's own.
    pub env: &'a HashMap<String, String>,
    pub working_dir: &'a str,
    pub model_label: &'a str,
    pub session_id: &'a str,
    pub db: &'a crate::db::Db,
    pub broadcaster: &'a crate::ws::broadcaster::Broadcaster,
    pub timeout_secs: u64,
    pub cancel: Arc<Notify>,
    /// Signalled by [`crate::provider::agent::AgentProvider::shutdown_after_turn`]:
    /// the work this turn was dispatched for is already done — its card was
    /// transitioned by a terminal MCP tool — so the agent should wrap up.
    ///
    /// The in-flight tool response and any closing assistant text still reach
    /// the transcript for [`RETIRE_GRACE_SECS`]; after that the child is wound
    /// down through the same SIGINT-then-drain path a cancel uses, and the
    /// turn is reported as a clean completion rather than a crash or an
    /// interrupt — the transition it was working towards did succeed.
    pub retire: Arc<Notify>,
    /// Grace allowed after [`retire`](Self::retire) fires, normally
    /// [`RETIRE_GRACE_SECS`]. A field rather than a bare const so tests can
    /// exercise the expiry path without sleeping for the real window.
    pub retire_grace_secs: u64,
    /// Auth-failure (and any other) stderr markers, as data.
    pub stderr_markers: &'static [StderrMarker],
    /// Extra sentence appended to the "failed to spawn" crash reason, e.g.
    /// an install hint.
    pub spawn_hint: Option<&'a str>,
    /// Crash reason for a non-zero exit that printed nothing to stderr.
    pub empty_exit_reason: &'a str,
    /// Emit `Started` before the stream opens. False for a CLI whose stream
    /// carries an init frame the parser turns into `Started` itself.
    pub started_up_front: bool,
    /// Treat a non-zero exit that nevertheless produced events as a
    /// completion — `cursor-agent` exits non-zero on some clean turns.
    pub success_on_output: bool,
    /// Plugin host for this turn, when the dispatching `SessionManager` was
    /// built with `with_plugins`. Every assistant `Text` event the parser
    /// produces is additionally handed to any `todo`-hook plugin (see
    /// [`crate::plugin::todo_hook::emit_plugin_todos`]) so it can drive todo
    /// lifecycle tracking for a provider that has no native `TodoWrite`.
    /// `None` (and a plugin host with no `todo` listener) are both no-ops.
    pub plugins: Option<&'a crate::plugin::manager::PluginManager>,
}
/// Outcome of one turn: what to report to the session manager.
pub struct TurnResult {
    /// True only on a clean completion (no crash, no cancel).
    pub completed: bool,
    /// The crash reason, when the turn crashed. Threaded into
    /// [`crate::provider::agent::ProcessCompletion::error`] so a failed
    /// handover rollback can show the user *why*.
    pub error: Option<String>,
    /// Classification of `error`, threaded into
    /// [`crate::provider::agent::ProcessCompletion::error_kind`]. `None`
    /// whenever `error` is `None`.
    pub error_kind: Option<CrashKind>,
}

impl TurnResult {
    fn ok() -> Self {
        TurnResult {
            completed: true,
            error: None,
            error_kind: None,
        }
    }
    /// A turn that ended without completing but without a reportable error
    /// either — i.e. the user cancelled it.
    fn cancelled() -> Self {
        TurnResult {
            completed: false,
            error: None,
            error_kind: None,
        }
    }
    fn failed(reason: impl Into<String>, kind: CrashKind) -> Self {
        TurnResult {
            completed: false,
            error: Some(reason.into()),
            error_kind: Some(kind),
        }
    }
}

/// Why the stdout loop stopped.
enum TurnOutcome {
    Eof,
    Cancelled,
    /// `shutdown_after_turn` fired and the child was still going when the
    /// retire grace ran out.
    Retired,
    /// An `abort` stderr marker fired.
    Aborted,
    Timeout,
    ReadError(String),
}

/// Spawn the CLI for one turn, stream its stdout through `stream` into
/// provider events, and emit exactly one terminal `Completed` / `Crashed`.
pub async fn run_turn(spec: TurnSpec<'_>, stream: &mut dyn TurnStream) -> TurnResult {
    let TurnSpec {
        provider,
        cli_path,
        args: cli_args,
        env,
        working_dir,
        model_label,
        session_id,
        db,
        broadcaster,
        timeout_secs,
        cancel,
        retire,
        retire_grace_secs,
        stderr_markers,
        spawn_hint,
        empty_exit_reason,
        started_up_front,
        success_on_output,
        plugins,
    } = spec;

    tracing::info!(
        session_id = %session_id,
        "Spawning {provider}: {} {}",
        cli_path,
        cli_args.join(" ")
    );

    let mut cmd = Command::new(cli_path);
    cmd.args(cli_args)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in env {
        cmd.env(key, value);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            // Surface a Started→Crashed pair so the UI shows the failure
            // rather than a silent no-op.
            emit_started(db, broadcaster, session_id, model_label, provider).await;
            let mut reason = format!("failed to spawn '{cli_path}': {e}");
            if let Some(hint) = spawn_hint {
                reason.push_str(". ");
                reason.push_str(hint);
            }
            crash(
                db,
                broadcaster,
                session_id,
                &reason,
                CrashKind::SpawnFailed,
                None,
            )
            .await;
            return TurnResult::failed(reason, CrashKind::SpawnFailed);
        }
    };

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take();

    // Drain stderr concurrently: accumulate a bounded buffer for crash
    // reporting AND notify on any `abort` marker (an unauthenticated CLI
    // that would otherwise block forever on an interactive login prompt).
    // Buffer and matched markers live in shared state (not the task's
    // return value) so they stay readable even when the drain task itself
    // outlives the child — a grandchild holding the write end keeps the
    // task pending past the bounded join below.
    let abort_notify = Arc::new(Notify::new());
    let abort_setter = abort_notify.clone();
    let stderr_state: Arc<std::sync::Mutex<(String, Vec<usize>)>> = Arc::default();
    let stderr_task = stderr.map(|s| {
        let state = stderr_state.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(s).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut st = state.lock().expect("stderr state poisoned");
                let (buf, matched) = &mut *st;
                for (i, m) in stderr_markers.iter().enumerate() {
                    if !matched.contains(&i) && line.contains(m.marker) {
                        matched.push(i);
                        if m.abort {
                            abort_setter.notify_one();
                        }
                    }
                }
                if buf.len() < MAX_STDERR_BYTES {
                    if !buf.is_empty() {
                        buf.push('\n');
                    }
                    buf.push_str(&line);
                }
            }
        })
    });

    if started_up_front {
        emit_started(db, broadcaster, session_id, model_label, provider).await;
    }

    let mut saw_any = false;
    let mut lines = BufReader::new(stdout).lines();
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs));
    tokio::pin!(deadline);
    // Armed only once `retire` fires; until then the branch that awaits it is
    // switched off by its `if retiring` guard.
    let retire_grace = tokio::time::sleep(std::time::Duration::from_secs(retire_grace_secs));
    tokio::pin!(retire_grace);
    let mut retiring = false;

    let outcome = loop {
        tokio::select! {
            _ = cancel.notified() => {
                break graceful_cancel(
                    &mut child,
                    &mut lines,
                    stream,
                    db,
                    broadcaster,
                    session_id,
                    provider,
                    plugins,
                    &mut saw_any,
                )
                .await;
            }
            _ = retire.notified(), if !retiring => {
                // The card this turn was working has already been
                // transitioned, so everything from here on is spend nobody
                // will read. Give the agent a short window to take the
                // in-flight tool response and finish its sentence.
                tracing::info!(
                    session_id = %session_id,
                    "{provider}: retiring turn after its card was transitioned"
                );
                retiring = true;
                let grace = std::time::Duration::from_secs(retire_grace_secs);
                retire_grace
                    .as_mut()
                    .reset(tokio::time::Instant::now() + grace);
            }
            _ = &mut retire_grace, if retiring => {
                let _ = graceful_cancel(
                    &mut child,
                    &mut lines,
                    stream,
                    db,
                    broadcaster,
                    session_id,
                    provider,
                    plugins,
                    &mut saw_any,
                )
                .await;
                break TurnOutcome::Retired;
            }
            _ = abort_notify.notified() => {
                let _ = child.start_kill();
                break TurnOutcome::Aborted;
            }
            _ = &mut deadline => {
                let _ = child.start_kill();
                break TurnOutcome::Timeout;
            }
            line = lines.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        emit_line_events(
                            &line,
                            stream,
                            db,
                            broadcaster,
                            session_id,
                            provider,
                            plugins,
                            &mut saw_any,
                        )
                        .await;
                    }
                    Ok(None) => break TurnOutcome::Eof,
                    Err(e) => {
                        tracing::warn!(
                            session_id = %session_id,
                            "{provider}: stdout read error: {e}"
                        );
                        let _ = child.start_kill();
                        break TurnOutcome::ReadError(e.to_string());
                    }
                }
            }
        }
    };

    // Let the child fully exit and collect its status + stderr.
    let status = child.wait().await.ok();
    let (stderr_text, matched) = {
        if let Some(t) = stderr_task {
            // Bounded join: a grandchild that inherited the stderr write
            // end (a tool call's daemon) can hold the pipe open after the
            // child is gone; without the cap the drain never EOFs and the
            // turn never returns. Mirrors the 2s guard in claude/process.
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), t).await;
        }
        let st = stderr_state.lock().expect("stderr state poisoned");
        (st.0.trim().to_string(), st.1.clone())
    };
    // Markers matched on stderr, resolved back to their definitions.
    let matched: Vec<&StderrMarker> = matched.into_iter().map(|i| &stderr_markers[i]).collect();

    // A parser that emitted its own Started satisfies the UI already; every
    // terminal branch below needs one to have been emitted either way.
    if !started_up_front && !stream.emitted_start() {
        emit_started(db, broadcaster, session_id, model_label, provider).await;
    }

    match outcome {
        TurnOutcome::Eof => {
            let ok = status.map(|s| s.success()).unwrap_or(false);
            if let Some(reason) = stream.take_error() {
                // An error frame inside the stream: its text is all we have
                // to go on.
                let kind = CrashKind::classify(&reason);
                crash(
                    db,
                    broadcaster,
                    session_id,
                    &reason,
                    kind,
                    exit_code(status),
                )
                .await;
                TurnResult::failed(reason, kind)
            } else if ok || (success_on_output && saw_any) {
                emit_event(
                    db,
                    broadcaster,
                    session_id,
                    ProviderEvent::Completed {
                        conversation_id: stream.take_conversation_id(),
                        result_meta: serde_json::Value::Null,
                    },
                )
                .await;
                TurnResult::ok()
            } else {
                // A matched marker carries its own classification;
                // otherwise sniff the stderr tail, and a silent non-zero
                // exit is `no_output` by construction.
                let (reason, kind) = match matched.iter().find(|m| !m.abort) {
                    Some(m) => (m.message.to_string(), m.kind),
                    None if stderr_text.is_empty() => {
                        (empty_exit_reason.to_string(), CrashKind::NoOutput)
                    }
                    None => {
                        let kind = CrashKind::classify(&stderr_text);
                        (stderr_text, kind)
                    }
                };
                crash(
                    db,
                    broadcaster,
                    session_id,
                    &reason,
                    kind,
                    exit_code(status),
                )
                .await;
                TurnResult::failed(reason, kind)
            }
        }
        TurnOutcome::Cancelled => {
            // The interrupt route appends its own `interrupt` event; emit a
            // Completed so any in-flight tool spinner closes and the
            // orchestrator sees a clean end.
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::Completed {
                    conversation_id: stream.take_conversation_id(),
                    result_meta: serde_json::Value::Null,
                },
            )
            .await;
            TurnResult::cancelled()
        }
        TurnOutcome::Retired => {
            // Neither a crash nor a user interrupt: the card was already
            // transitioned by a terminal MCP tool, so the work this turn
            // existed to do succeeded. Report a clean completion.
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::Completed {
                    conversation_id: stream.take_conversation_id(),
                    result_meta: serde_json::Value::Null,
                },
            )
            .await;
            TurnResult::ok()
        }
        TurnOutcome::Aborted => {
            let (reason, kind) = matched
                .iter()
                .find(|m| m.abort)
                .map(|m| (m.message.to_string(), m.kind))
                .unwrap_or_else(|| (empty_exit_reason.to_string(), CrashKind::Unknown));
            crash(db, broadcaster, session_id, &reason, kind, None).await;
            TurnResult::failed(reason, kind)
        }
        TurnOutcome::Timeout => {
            let reason = format!("{provider} turn exceeded {timeout_secs}s timeout");
            crash(
                db,
                broadcaster,
                session_id,
                &reason,
                CrashKind::Timeout,
                None,
            )
            .await;
            TurnResult::failed(reason, CrashKind::Timeout)
        }
        TurnOutcome::ReadError(e) => {
            let reason = format!("stdout read error: {e}");
            crash(
                db,
                broadcaster,
                session_id,
                &reason,
                CrashKind::Unknown,
                None,
            )
            .await;
            TurnResult::failed(reason, CrashKind::Unknown)
        }
    }
}

/// Parse one raw stdout line into provider events, persist each one, and
/// feed assistant text to the todo hook — shared by the main stdout loop
/// and [`graceful_cancel`]'s post-signal drain, so a line the CLI writes
/// during the interrupt grace window is handled identically to one from
/// mid-turn.
#[allow(clippy::too_many_arguments)]
async fn emit_line_events(
    line: &str,
    stream: &mut dyn TurnStream,
    db: &crate::db::Db,
    broadcaster: &crate::ws::broadcaster::Broadcaster,
    session_id: &str,
    provider: &str,
    plugins: Option<&crate::plugin::manager::PluginManager>,
    saw_any: &mut bool,
) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        // Non-JSON noise on stdout — log and skip.
        tracing::debug!(
            session_id = %session_id,
            "{provider}: non-JSON stdout line ignored"
        );
        return;
    };
    for event in stream.on_line(&json) {
        // `Started` alone is not "output" for `success_on_output`:
        // cursor-agent prints its init frame before failing turns too, and
        // the parser's synthetic Started must not turn that failure into a
        // clean completion.
        *saw_any |= !matches!(event, ProviderEvent::Started { .. });
        // Assistant text is the one thing a todo-hook plugin can parse;
        // clone it before the event is moved into the persistence path.
        let todo_text = match (&event, plugins) {
            (ProviderEvent::Text { text }, Some(_)) => Some(text.clone()),
            _ => None,
        };
        emit_event(db, broadcaster, session_id, event).await;
        if let (Some(text), Some(plugins)) = (todo_text, plugins) {
            crate::plugin::todo_hook::emit_plugin_todos(
                plugins,
                db,
                broadcaster,
                session_id,
                crate::plugin::todo_hook::assistant_text_payload(provider, &text),
            )
            .await;
        }
    }
}

/// Grace window after SIGINT before giving up and SIGKILLing: long enough
/// for a CLI to flush its last buffered stdout frame, short enough that an
/// interrupt still feels immediate.
const INTERRUPT_DRAIN_SECS: u64 = 2;

/// Send SIGINT to the child. `true` when a signal was actually delivered (a
/// pid was available); the caller hard-kills immediately when this returns
/// `false` instead of waiting out a grace window for nothing.
#[cfg(unix)]
async fn send_interrupt_signal(child: &tokio::process::Child) -> bool {
    match child.id() {
        Some(pid) => {
            // SAFETY: `kill` with a pid tokio just reported for this child
            // and the standard SIGINT signal number; ESRCH (already exited)
            // is a harmless race — the drain loop below handles that case
            // either way.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGINT);
            }
            true
        }
        None => false,
    }
}

#[cfg(not(unix))]
async fn send_interrupt_signal(_child: &tokio::process::Child) -> bool {
    // No portable "ask nicely" signal outside unix — the caller falls back
    // to an immediate hard kill.
    false
}

/// Cancel path for `cursor`/`grok`/`kimi`: none of the three CLIs exposes an
/// in-band control channel (no stdin — see `TurnSpec`'s `Stdio::null()`),
/// so there is no way to ask the run to wind down cleanly. The next best
/// thing is SIGINT plus a short drain: many CLIs (and the shells/tools they
/// spawn) treat SIGINT as "stop after the current step" and flush one more
/// stdout frame before exiting, which — unlike a bare SIGKILL — still
/// reaches `stream` and gets persisted. If nothing arrives within
/// `INTERRUPT_DRAIN_SECS` (the CLI ignored the signal, or there's no pid to
/// signal at all on a non-unix host), this falls back to the same
/// `start_kill` the hard-kill path always used. `InterruptKind::HardKill`
/// stays the declared capability regardless — the UI still promises a hard
/// stop; this only reduces how much output that stop throws away.
#[allow(clippy::too_many_arguments)]
async fn graceful_cancel(
    child: &mut tokio::process::Child,
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    stream: &mut dyn TurnStream,
    db: &crate::db::Db,
    broadcaster: &crate::ws::broadcaster::Broadcaster,
    session_id: &str,
    provider: &str,
    plugins: Option<&crate::plugin::manager::PluginManager>,
    saw_any: &mut bool,
) -> TurnOutcome {
    if !send_interrupt_signal(child).await {
        let _ = child.start_kill();
        return TurnOutcome::Cancelled;
    }

    let grace = tokio::time::sleep(std::time::Duration::from_secs(INTERRUPT_DRAIN_SECS));
    tokio::pin!(grace);
    loop {
        tokio::select! {
            _ = &mut grace => {
                let _ = child.start_kill();
                return TurnOutcome::Cancelled;
            }
            line = lines.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        emit_line_events(
                            &line, stream, db, broadcaster, session_id, provider, plugins, saw_any,
                        )
                        .await;
                    }
                    Ok(None) | Err(_) => {
                        let _ = child.start_kill();
                        return TurnOutcome::Cancelled;
                    }
                }
            }
        }
    }
}

fn exit_code(status: Option<std::process::ExitStatus>) -> Option<i32> {
    status.and_then(|s| s.code())
}

async fn emit_started(
    db: &crate::db::Db,
    broadcaster: &crate::ws::broadcaster::Broadcaster,
    session_id: &str,
    model_label: &str,
    provider: &str,
) {
    emit_event(
        db,
        broadcaster,
        session_id,
        ProviderEvent::Started {
            model: model_label.to_string(),
            conversation_id: None,
            metadata: serde_json::json!({ "provider": provider }),
        },
    )
    .await;
}

async fn crash(
    db: &crate::db::Db,
    broadcaster: &crate::ws::broadcaster::Broadcaster,
    session_id: &str,
    reason: &str,
    error_kind: CrashKind,
    exit_code: Option<i32>,
) {
    emit_event(
        db,
        broadcaster,
        session_id,
        ProviderEvent::Crashed {
            reason: reason.to_string(),
            error_kind,
            exit_code,
            stderr: None,
        },
    )
    .await;
}

/// Tell the user, in the chat, that attachments on their message were not
/// sent. None of `cursor-agent`, `grok` or `kimi` exposes an image-input
/// flag (verified against the installed CLIs), so a turn with attachments
/// silently answered the text alone — which reads as the model ignoring the
/// screenshot. Emitting it as assistant text is the only channel the
/// per-turn providers have into the transcript.
pub async fn notify_attachments_dropped(
    db: &crate::db::Db,
    broadcaster: &crate::ws::broadcaster::Broadcaster,
    session_id: &str,
    provider: &str,
    count: usize,
) {
    let plural = if count == 1 { "" } else { "s" };
    tracing::warn!(
        session_id = %session_id,
        "{provider}: dropping {count} attachment{plural} — the {provider} CLI is text-only"
    );
    emit_event(
        db,
        broadcaster,
        session_id,
        ProviderEvent::Text {
            text: format!(
                "**Note:** {count} attachment{plural} on your message {} not sent — \
                 the `{provider}` CLI accepts text only. Save the file into the \
                 working directory and reference it by path, or switch the session \
                 to a provider with image support (Claude, Ollama).",
                if count == 1 { "was" } else { "were" }
            ),
        },
    )
    .await;
}

/// The system prompt for a provider with no Claude-style
/// `--append-system-prompt`: the shared working-style rules, then any
/// per-spawn suffix (repeating-task context), then any per-session custom
/// prompt. Mirrors how the Claude provider layers the same three sources
/// (`claude/mod.rs`) so a session behaves the same whichever CLI runs it —
/// before this, only Claude honoured `system_prompt_suffix` at all, and
/// cursor honoured neither.
pub fn compose_system_prompt(config: &SpawnConfig) -> String {
    let mut prompt = crate::provider::WORKING_STYLE.to_string();
    for extra in [
        config.system_prompt_suffix.as_deref(),
        config.system_prompt_override.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .filter(|s| !s.is_empty())
    {
        prompt.push('\n');
        prompt.push_str(extra);
    }
    prompt
}

/// Resolve the executable to spawn. A configured path containing a `/` is
/// used verbatim. A bare name is kept when it resolves on the server's
/// PATH; otherwise the first `fallback_dirs` entry holding an executable of
/// that name wins. This matters because the PeckBoard server often runs
/// with a service PATH that predates the CLI install (installers typically
/// only extend `~/.bashrc`). A `~/` prefix in a fallback entry expands to
/// `$HOME`.
pub fn resolve_cli_path(configured: &str, fallback_dirs: &[&str]) -> String {
    let path_var = std::env::var("PATH").unwrap_or_default();
    let home = std::env::var("HOME").ok();
    resolve_cli_path_in(configured, &path_var, home.as_deref(), fallback_dirs)
}

pub(crate) fn resolve_cli_path_in(
    configured: &str,
    path_var: &str,
    home: Option<&str>,
    fallback_dirs: &[&str],
) -> String {
    if configured.contains('/') {
        return configured.to_string();
    }
    let on_path = std::env::split_paths(path_var).any(|dir| dir.join(configured).is_file());
    if on_path {
        return configured.to_string();
    }
    for dir in fallback_dirs {
        let dir = match dir.strip_prefix("~/") {
            Some(rest) => match home {
                Some(home) => std::path::Path::new(home).join(rest),
                None => continue,
            },
            None => std::path::PathBuf::from(dir),
        };
        let candidate = dir.join(configured);
        if candidate.is_file() {
            return candidate.to_string_lossy().to_string();
        }
    }
    configured.to_string()
}

/// A trimmed, non-empty string setting.
pub fn setting_str(settings: &HashMap<String, serde_json::Value>, key: &str) -> Option<String> {
    settings
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// A boolean setting.
pub fn setting_bool(settings: &HashMap<String, serde_json::Value>, key: &str) -> Option<bool> {
    settings.get(key).and_then(|v| v.as_bool())
}

/// A `StringList` setting as a flat `Vec<String>`, trimming entries and
/// dropping blanks. Empty when unset.
pub fn setting_str_list(settings: &HashMap<String, serde_json::Value>, key: &str) -> Vec<String> {
    let Some(arr) = settings.get(key).and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Append the user's `additional_models` to a base catalog (the
/// autodiscovered list or the static seed), skipping ids already present so
/// a duplicate never shows twice. `make` builds the `ModelInfo` for a bare
/// id — each provider labels and tags its own models.
pub fn merge_additional_models(
    base: Vec<ModelInfo>,
    extras: Vec<String>,
    make: impl Fn(String) -> ModelInfo,
) -> Vec<ModelInfo> {
    let mut seen: std::collections::HashSet<String> = base.iter().map(|m| m.id.clone()).collect();
    let mut models = base;
    for name in extras {
        if seen.insert(name.clone()) {
            models.push(make(name));
        }
    }
    models
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::ws::broadcaster::Broadcaster;

    /// A stream that records raw lines; the harness tests care about the
    /// process plumbing, not any one CLI's JSON dialect.
    #[derive(Default)]
    struct EchoStream {
        conversation_id: Option<String>,
        error: Option<String>,
        seen: Vec<String>,
    }

    impl TurnStream for EchoStream {
        fn on_line(&mut self, json: &serde_json::Value) -> Vec<ProviderEvent> {
            if let Some(text) = json.get("text").and_then(|v| v.as_str()) {
                self.seen.push(text.to_string());
                return vec![ProviderEvent::Text {
                    text: text.to_string(),
                }];
            }
            if json.get("start").is_some() {
                return vec![ProviderEvent::Started {
                    model: "test:model".into(),
                    conversation_id: None,
                    metadata: serde_json::Value::Null,
                }];
            }
            Vec::new()
        }
        fn take_conversation_id(&mut self) -> Option<String> {
            self.conversation_id.take()
        }
        fn take_error(&mut self) -> Option<String> {
            self.error.take()
        }
    }

    const NO_MARKERS: &[StderrMarker] = &[];

    fn spec<'a>(
        cli_path: &'a str,
        args: &'a [String],
        env: &'a HashMap<String, String>,
        db: &'a Db,
        broadcaster: &'a Broadcaster,
        cancel: Arc<Notify>,
        timeout_secs: u64,
    ) -> TurnSpec<'a> {
        TurnSpec {
            provider: "test",
            cli_path,
            args,
            env,
            working_dir: ".",
            model_label: "test:model",
            session_id: "s1",
            db,
            broadcaster,
            timeout_secs,
            cancel,
            stderr_markers: NO_MARKERS,
            spawn_hint: None,
            empty_exit_reason: "test CLI exited without a successful result",
            started_up_front: true,
            success_on_output: false,
            // Never signalled unless a test overrides it via struct-update
            // syntax; the short grace keeps the expiry path testable.
            retire: Arc::new(Notify::new()),
            retire_grace_secs: 1,
            plugins: None,
        }
    }

    async fn session_db() -> Db {
        let db = Db::in_memory().unwrap();
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(crate::db::models::NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: ".".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_session(crate::db::models::NewSession {
            id: "s1".into(),
            name: "S".into(),
            folder_id: "f1".into(),
            created_at: ts.clone(),
            last_activity: ts,
            ..Default::default()
        })
        .await
        .unwrap();
        db
    }

    /// The bug this harness was extracted to fix: `cursor-agent` had no
    /// turn timeout, so a wedged child ran forever. Every provider on the
    /// harness now gets one.
    #[tokio::test]
    async fn hung_child_is_killed_at_the_turn_timeout() {
        let db = session_db().await;
        let broadcaster = Broadcaster::new();
        let env = HashMap::new();
        let args = vec!["30".to_string()];
        let mut stream = EchoStream::default();

        let started = std::time::Instant::now();
        let result = run_turn(
            spec(
                "sleep",
                &args,
                &env,
                &db,
                &broadcaster,
                Arc::new(Notify::new()),
                1,
            ),
            &mut stream,
        )
        .await;

        assert!(!result.completed);
        assert_eq!(
            result.error.as_deref(),
            Some("test turn exceeded 1s timeout"),
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "the turn should end at its own deadline, not the child's",
        );
    }

    /// SIGINT-then-drain: `cursor`/`grok`/`kimi` have no stdin channel, so
    /// there is no in-band way to ask a run to wind down — the harness
    /// signals instead. A CLI that traps the signal and emits one more
    /// frame before exiting gets that frame into the transcript rather
    /// than losing it to a bare SIGKILL.
    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_drains_a_frame_the_child_emits_after_sigint() {
        let db = session_db().await;
        let broadcaster = Broadcaster::new();
        let env = HashMap::new();
        // bash defers running a trap until the *foreground command*
        // returns, so a signal caught while blocked in an external `sleep`
        // would sit unhandled until that child's own timer expired — not
        // a useful stand-in for a CLI's own signal handler. A builtin-only
        // busy loop has no such gap: bash checks for a pending trap after
        // every simple command, so `on_int` runs within microseconds of
        // the signal, same as a real CLI's async signal handler would.
        //
        // The child touches `ready` once its trap is installed, and the
        // canceller waits for that file rather than for a fixed delay: a
        // SIGINT that arrives before `trap` runs takes SIGINT's default
        // action and kills bash outright, so a timing assumption here makes
        // the test fail on a loaded machine for a reason the code under
        // test has nothing to do with.
        // The name stays in the shell-safe charset (digits and dashes) so it
        // can go into the script unquoted.
        let ready = std::env::temp_dir().join(format!(
            "peckboard-sigint-ready-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        let _ = std::fs::remove_file(&ready);
        let script = format!(
            "stop=0\non_int() {{ printf '{{\"text\":\"post-sigint\"}}\\n'; stop=1; }}\ntrap on_int INT\n: > {ready}\nwhile [ \"$stop\" = 0 ]; do :; done\nexit 0\n",
            ready = ready.display(),
        );
        let args = vec!["-c".to_string(), script];
        let mut stream = EchoStream::default();
        let cancel = Arc::new(Notify::new());
        let notifier = cancel.clone();
        let ready_path = ready.clone();
        tokio::spawn(async move {
            // Cap the wait so a child that never starts fails the assertions
            // below rather than hanging the test.
            for _ in 0..1000 {
                if ready_path.exists() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            notifier.notify_one();
        });

        let result = run_turn(
            spec("bash", &args, &env, &db, &broadcaster, cancel, 30),
            &mut stream,
        )
        .await;
        let _ = std::fs::remove_file(&ready);
        assert!(!result.completed);
        assert_eq!(stream.seen, vec!["post-sigint".to_string()]);
        assert_eq!(stream.seen, vec!["post-sigint".to_string()]);
        let kinds: Vec<String> = db
            .events_tail("s1", 100)
            .await
            .unwrap()
            .iter()
            .map(|e| e.kind.clone())
            .collect();
        assert!(
            kinds.iter().any(|k| k == "agent-text"),
            "the frame the child emitted after SIGINT reached the transcript: {kinds:?}",
        );
    }

    /// A child that ignores SIGINT (`trap '' INT`) still gets stopped —
    /// just later, after the drain grace window expires and the harness
    /// falls back to SIGKILL.
    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_hard_kills_after_grace_window_when_child_ignores_sigint() {
        let db = session_db().await;
        let broadcaster = Broadcaster::new();
        let env = HashMap::new();
        let script = "trap '' INT\nsleep 3\n";
        let args = vec!["-c".to_string(), script.to_string()];
        let mut stream = EchoStream::default();
        let cancel = Arc::new(Notify::new());
        let notifier = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            notifier.notify_one();
        });

        let started = std::time::Instant::now();
        let result = run_turn(
            spec("bash", &args, &env, &db, &broadcaster, cancel, 30),
            &mut stream,
        )
        .await;
        let elapsed = started.elapsed();

        assert!(!result.completed);
        assert!(
            elapsed >= std::time::Duration::from_secs(INTERRUPT_DRAIN_SECS),
            "should wait out the full grace window before hard-killing: {elapsed:?}",
        );
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "hard kill should follow the grace window promptly: {elapsed:?}",
        );
    }

    /// `shutdown_after_turn` must not truncate the agent mid-thought: the
    /// grace window exists precisely so the in-flight tool response and the
    /// closing assistant text still reach the transcript. A child that wraps
    /// up inside the window ends on the ordinary EOF path.
    #[tokio::test]
    async fn retire_lets_a_child_that_wraps_up_inside_the_grace_finish_normally() {
        let db = session_db().await;
        let broadcaster = Broadcaster::new();
        let env = HashMap::new();
        let script = "printf '{\"text\":\"first\"}\\n'\nsleep 0.3\nprintf '{\"text\":\"second\"}\\n'\nexit 0\n";
        let args = vec!["-c".to_string(), script.to_string()];
        let mut stream = EchoStream::default();
        let retire = Arc::new(Notify::new());
        let notifier = retire.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            notifier.notify_one();
        });

        let result = run_turn(
            TurnSpec {
                retire,
                ..spec(
                    "bash",
                    &args,
                    &env,
                    &db,
                    &broadcaster,
                    Arc::new(Notify::new()),
                    30,
                )
            },
            &mut stream,
        )
        .await;

        assert!(result.completed, "error: {:?}", result.error);
        assert_eq!(
            stream.seen,
            vec!["first".to_string(), "second".to_string()],
            "text written after the retire signal must still be streamed",
        );
    }

    /// A child still going when the grace expires is wound down through the
    /// same SIGINT-then-drain path a cancel uses — but the turn is reported
    /// as a COMPLETION, not a crash and not an interrupt. Its card was
    /// already transitioned by the terminal MCP tool that triggered the
    /// retire, so the work this turn existed to do did succeed; reporting a
    /// crash here would fire the auto-pause counter on a healthy worker.
    ///
    /// A builtin-only busy loop, not `sleep`: bash defers a signal until the
    /// current foreground *external* command returns, which would hide how
    /// promptly the wind-down actually lands.
    #[cfg(unix)]
    #[tokio::test]
    async fn retire_winds_down_an_overrunning_child_and_still_reports_completion() {
        let db = session_db().await;
        let broadcaster = Broadcaster::new();
        let env = HashMap::new();
        let script = "printf '{\"text\":\"still-working\"}\\n'\nwhile :; do :; done\n";
        let args = vec!["-c".to_string(), script.to_string()];
        let mut stream = EchoStream::default();
        let retire = Arc::new(Notify::new());
        let notifier = retire.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            notifier.notify_one();
        });

        let result = run_turn(
            TurnSpec {
                retire,
                ..spec(
                    "bash",
                    &args,
                    &env,
                    &db,
                    &broadcaster,
                    Arc::new(Notify::new()),
                    30,
                )
            },
            &mut stream,
        )
        .await;

        assert!(
            result.completed,
            "a retired turn is a completion, not a crash: {:?}",
            result.error
        );
        assert!(result.error.is_none());
        assert!(result.error_kind.is_none());
        assert_eq!(stream.seen, vec!["still-working".to_string()]);
        let kinds: Vec<String> = db
            .events_tail("s1", 100)
            .await
            .unwrap()
            .iter()
            .map(|e| e.kind.clone())
            .collect();
        assert_eq!(
            kinds.last().map(String::as_str),
            Some("agent-end"),
            "the turn must terminate with a completion event: {kinds:?}",
        );
    }
    /// The plugin todo hook on the shared harness — the seam `cursor`,
    /// `grok` and `kimi` all reach through. Every assistant `Text` event is
    /// handed to the `todo` hook; with no todo-hook plugin installed that
    /// dispatch short-circuits, so the turn's event stream is byte-for-byte
    /// what it was before the wiring existed.
    ///
    /// The `allow`-payload → `todo` event half of the seam (which needs a
    /// live wasm plugin) is covered host-side in
    /// `tests/plugin_todo_lifecycle.rs`.
    #[tokio::test]
    async fn assistant_text_runs_through_the_todo_hook_without_perturbing_the_stream() {
        let db = session_db().await;
        let broadcaster = Broadcaster::new();
        let env = HashMap::new();
        let args = vec![r#"{"text":"Ship the feature"}"#.to_string()];
        let mut stream = EchoStream::default();
        let plugins = crate::plugin::manager::PluginManager::empty();

        let mut turn_spec = spec(
            "echo",
            &args,
            &env,
            &db,
            &broadcaster,
            Arc::new(Notify::new()),
            30,
        );
        turn_spec.plugins = Some(&plugins);
        let result = run_turn(turn_spec, &mut stream).await;

        assert!(result.completed, "error: {:?}", result.error);
        assert_eq!(stream.seen, vec!["Ship the feature".to_string()]);

        let kinds: Vec<String> = db
            .events_tail("s1", 100)
            .await
            .unwrap()
            .iter()
            .map(|e| e.kind.clone())
            .collect();
        assert!(
            kinds.iter().any(|k| k == "agent-text"),
            "the assistant text still reaches the transcript: {kinds:?}",
        );
        assert!(
            !kinds.iter().any(|k| k == "todo"),
            "no todo-hook plugin -> no todo event: {kinds:?}",
        );
    }

    /// `SpawnConfig::env` reaches the child. Cursor used to forward only
    /// its MCP token and drop the rest of the map.
    #[tokio::test]
    async fn env_is_forwarded_to_the_child() {
        let db = session_db().await;
        let broadcaster = Broadcaster::new();
        let mut env = HashMap::new();
        env.insert("PECKBOARD_TEST_VAR".to_string(), "hello".to_string());
        let args = vec![
            "-c".to_string(),
            r#"printf '{"text":"%s"}\n' "$PECKBOARD_TEST_VAR""#.to_string(),
        ];
        let mut stream = EchoStream::default();

        let result = run_turn(
            spec(
                "sh",
                &args,
                &env,
                &db,
                &broadcaster,
                Arc::new(Notify::new()),
                30,
            ),
            &mut stream,
        )
        .await;

        assert!(result.completed, "error: {:?}", result.error);
        assert_eq!(stream.seen, vec!["hello".to_string()]);
    }

    /// A crash reason is returned so the caller can put it on
    /// `ProcessCompletion.error` — non-Claude handover rollbacks used to
    /// show no cause at all.
    #[tokio::test]
    async fn failed_exit_reports_stderr_as_the_error() {
        let db = session_db().await;
        let broadcaster = Broadcaster::new();
        let env = HashMap::new();
        let args = vec!["-c".to_string(), "echo boom >&2; exit 3".to_string()];
        let mut stream = EchoStream::default();

        let result = run_turn(
            spec(
                "sh",
                &args,
                &env,
                &db,
                &broadcaster,
                Arc::new(Notify::new()),
                30,
            ),
            &mut stream,
        )
        .await;

        assert!(!result.completed);
        assert_eq!(result.error.as_deref(), Some("boom"));
        let events = db.events_tail("s1", 10).await.unwrap();
        assert_eq!(events.last().unwrap().kind, "agent-end");
    }

    /// An `abort` marker fast-fails the turn with its own copy instead of
    /// letting an interactive login prompt hang the child to the timeout.
    #[tokio::test]
    async fn abort_marker_fast_fails_with_its_message() {
        const MARKERS: &[StderrMarker] = &[StderrMarker {
            marker: "accounts.example/device",
            message: "This account isn't signed in.",
            kind: CrashKind::AuthExpired,
            abort: true,
        }];

        let db = session_db().await;
        let broadcaster = Broadcaster::new();
        let env = HashMap::new();
        let args = vec![
            "-c".to_string(),
            "echo 'open accounts.example/device' >&2; sleep 30".to_string(),
        ];
        let mut stream = EchoStream::default();
        let mut s = spec(
            "sh",
            &args,
            &env,
            &db,
            &broadcaster,
            Arc::new(Notify::new()),
            30,
        );
        s.stderr_markers = MARKERS;

        let result = run_turn(s, &mut stream).await;
        assert!(!result.completed);
        assert_eq!(
            result.error.as_deref(),
            Some("This account isn't signed in.")
        );
        assert_eq!(result.error_kind, Some(CrashKind::AuthExpired));
    }

    /// A non-abort marker only rewrites the reason once the CLI has exited
    /// non-zero (kimi's "No model configured").
    #[tokio::test]
    async fn exit_marker_rewrites_the_crash_reason() {
        const MARKERS: &[StderrMarker] = &[StderrMarker {
            marker: "No model configured",
            message: "Sign in first.",
            kind: CrashKind::AuthExpired,
            abort: false,
        }];

        let db = session_db().await;
        let broadcaster = Broadcaster::new();
        let env = HashMap::new();
        let args = vec![
            "-c".to_string(),
            "echo 'No model configured. run login' >&2; exit 1".to_string(),
        ];
        let mut stream = EchoStream::default();
        let mut s = spec(
            "sh",
            &args,
            &env,
            &db,
            &broadcaster,
            Arc::new(Notify::new()),
            30,
        );
        s.stderr_markers = MARKERS;

        let result = run_turn(s, &mut stream).await;
        assert_eq!(result.error.as_deref(), Some("Sign in first."));
    }

    /// Cursor's quirk, kept as a per-provider flag: a non-zero exit that
    /// still streamed events is a completion.
    #[tokio::test]
    async fn success_on_output_accepts_a_nonzero_exit_that_streamed() {
        let db = session_db().await;
        let broadcaster = Broadcaster::new();
        let env = HashMap::new();
        let args = vec![
            "-c".to_string(),
            r#"echo '{"text":"hi"}'; exit 1"#.to_string(),
        ];
        let mut stream = EchoStream::default();
        let mut s = spec(
            "sh",
            &args,
            &env,
            &db,
            &broadcaster,
            Arc::new(Notify::new()),
            30,
        );
        s.success_on_output = true;

        assert!(run_turn(s, &mut stream).await.completed);
    }

    /// A synthetic `Started` alone is not output: a CLI that prints its
    /// init frame and then dies non-zero must still report the crash
    /// even with `success_on_output` set.
    #[tokio::test]
    async fn success_on_output_ignores_a_started_only_stream() {
        let db = session_db().await;
        let broadcaster = Broadcaster::new();
        let env = HashMap::new();
        let args = vec![
            "-c".to_string(),
            r#"echo '{"start":true}'; echo boom >&2; exit 1"#.to_string(),
        ];
        let mut stream = EchoStream::default();
        let mut s = spec(
            "sh",
            &args,
            &env,
            &db,
            &broadcaster,
            Arc::new(Notify::new()),
            30,
        );
        s.success_on_output = true;

        let result = run_turn(s, &mut stream).await;
        assert!(!result.completed);
        assert_eq!(result.error.as_deref(), Some("boom"));
    }
    #[tokio::test]
    async fn spawn_failure_emits_started_then_crashed_with_the_hint() {
        let db = session_db().await;
        let broadcaster = Broadcaster::new();
        let env = HashMap::new();
        let args: Vec<String> = Vec::new();
        let mut stream = EchoStream::default();
        let mut s = spec(
            "peckboard-no-such-cli",
            &args,
            &env,
            &db,
            &broadcaster,
            Arc::new(Notify::new()),
            30,
        );
        s.spawn_hint = Some("Install it first.");

        let result = run_turn(s, &mut stream).await;
        assert!(!result.completed);
        let error = result.error.unwrap();
        assert!(
            error.starts_with("failed to spawn 'peckboard-no-such-cli'"),
            "{error}"
        );
        assert!(error.ends_with("Install it first."), "{error}");

        let kinds: Vec<String> = db
            .events_tail("s1", 10)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert_eq!(kinds, vec!["agent-start", "agent-end"]);
    }

    #[test]
    fn compose_system_prompt_layers_suffix_then_override() {
        let mut config = SpawnConfig::default();
        assert_eq!(
            compose_system_prompt(&config),
            crate::provider::WORKING_STYLE
        );

        config.system_prompt_suffix = Some("# Repeating Task Context".into());
        config.system_prompt_override = Some("Always answer in haiku.".into());
        let prompt = compose_system_prompt(&config);
        assert!(prompt.starts_with(crate::provider::WORKING_STYLE));
        let suffix_at = prompt.find("# Repeating Task Context").unwrap();
        let override_at = prompt.find("Always answer in haiku.").unwrap();
        assert!(suffix_at < override_at, "suffix must precede the override");

        // Blank values add nothing (not even a stray newline).
        config.system_prompt_suffix = Some("   ".into());
        config.system_prompt_override = Some(String::new());
        assert_eq!(
            compose_system_prompt(&config),
            crate::provider::WORKING_STYLE
        );
    }

    #[test]
    fn resolve_cli_path_prefers_path_then_fallback_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let bin = home.join(".demo-cli").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("demo"), "#!/bin/sh\n").unwrap();
        let home_str = home.to_str();

        // An explicit path is used verbatim.
        assert_eq!(
            resolve_cli_path_in("/opt/demo", "", None, &["~/.demo-cli/bin"]),
            "/opt/demo"
        );

        // On PATH → the bare name is kept.
        let on_path = home.join("path-bin");
        std::fs::create_dir_all(&on_path).unwrap();
        std::fs::write(on_path.join("demo"), "#!/bin/sh\n").unwrap();
        assert_eq!(
            resolve_cli_path_in(
                "demo",
                on_path.to_str().unwrap(),
                home_str,
                &["~/.demo-cli/bin"]
            ),
            "demo"
        );

        // Not on PATH → the fallback dir wins.
        assert_eq!(
            resolve_cli_path_in("demo", "", home_str, &["~/.demo-cli/bin"]),
            bin.join("demo").to_string_lossy(),
        );

        // Neither → unchanged, so the spawn error names what was tried.
        assert_eq!(
            resolve_cli_path_in("demo", "", home_str, &["~/.nowhere"]),
            "demo"
        );
    }

    #[test]
    fn merge_additional_models_dedups_against_the_base() {
        let base = vec![ModelInfo {
            id: "a".into(),
            display_name: "A".into(),
            capabilities: vec![],
            tier: 0,
        }];
        let merged =
            merge_additional_models(base, vec!["a".into(), "b".into(), "b".into()], |id| {
                ModelInfo {
                    display_name: format!("{id}!"),
                    id,
                    capabilities: vec![],
                    tier: 0,
                }
            });
        let ids: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
        assert_eq!(merged[1].display_name, "b!");
    }

    #[test]
    fn setting_accessors_trim_and_drop_blanks() {
        let settings: HashMap<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({
                "cli_path": "  demo  ",
                "blank": "   ",
                "flag": true,
                "list": ["  x ", "", "y"],
            }))
            .unwrap();
        assert_eq!(setting_str(&settings, "cli_path").as_deref(), Some("demo"));
        assert_eq!(setting_str(&settings, "blank"), None);
        assert_eq!(setting_str(&settings, "missing"), None);
        assert_eq!(setting_bool(&settings, "flag"), Some(true));
        assert_eq!(setting_str_list(&settings, "list"), vec!["x", "y"]);
        assert!(setting_str_list(&settings, "missing").is_empty());
    }
}
