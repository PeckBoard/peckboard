//! `terminal` capability: run a shell command, stream its output back as
//! [`AgentFrame::Event`] frames, and return the exit code.
//!
//! Output is streamed line-by-line (`kind: "terminal_output"`) so a session
//! sees progress live; the result payload also carries a bounded tail of
//! each stream for callers that only want the final blob. The child is
//! killed on cancel ([`ExecContext::cancel`]) or timeout, and spawned with
//! `kill_on_drop` so an early return never leaks a process.

use std::process::Stdio;
use std::time::Instant;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::executor::{CapabilityExecutor, ExecContext};

/// Default wall-clock cap on a command; overridable per request up to
/// [`MAX_TIMEOUT_SECS`].
const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 3600;
/// How many trailing lines of each stream to keep for the result payload.
/// Streaming carries the full output; this is only the summary tail.
const TAIL_LINES: usize = 200;

#[derive(Debug, Deserialize)]
struct TerminalArgs {
    /// The command line, run through the platform shell.
    command: String,
    /// Optional working directory.
    #[serde(default)]
    cwd: Option<String>,
    /// Optional per-request timeout (seconds), clamped to [`MAX_TIMEOUT_SECS`].
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// Runs a shell command with streamed output.
pub struct TerminalExecutor;

#[async_trait]
impl CapabilityExecutor for TerminalExecutor {
    async fn execute(&self, ctx: &ExecContext, args: Value) -> Result<Value, String> {
        let args: TerminalArgs =
            serde_json::from_value(args).map_err(|e| format!("invalid terminal args: {e}"))?;
        if args.command.trim().is_empty() {
            return Err("command must not be empty".to_string());
        }
        let timeout = args
            .timeout_secs
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS);
        run_command(ctx, &args.command, args.cwd.as_deref(), timeout).await
    }
}

/// Build a shell child with piped stdio. `sh -c` on unix, `cmd /C` on
/// windows. Shared with server management so a managed process and a
/// health-check command spawn the same way. Not yet spawned — caller may
/// tweak before `.spawn()`.
pub fn shell_command(command: &str, cwd: Option<&str>) -> Command {
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(command);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    };
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

/// Result of running a command to completion (or to its cancel/timeout).
pub struct RunOutcome {
    pub exit_code: Option<i32>,
    pub stdout_tail: Vec<String>,
    pub stderr_tail: Vec<String>,
    pub duration_ms: u128,
    pub cancelled: bool,
    pub timed_out: bool,
}

/// Spawn `command`, stream each output line as a `terminal_output` event,
/// and wait for it to finish — honoring the context's cancel token and the
/// `timeout`. Returns an error only if the child cannot be spawned; a
/// nonzero exit is a successful run with that exit code.
async fn run_command(
    ctx: &ExecContext,
    command: &str,
    cwd: Option<&str>,
    timeout: u64,
) -> Result<Value, String> {
    let started = Instant::now();
    let mut child = shell_command(command, cwd)
        .spawn()
        .map_err(|e| format!("failed to spawn command: {e}"))?;

    // Drain stdout+stderr concurrently into a single ordered channel of
    // (stream, line) so we can both stream events and keep a tail.
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (line_tx, mut line_rx) = mpsc::channel::<(&'static str, String)>(256);
    spawn_line_reader(stdout, "stdout", line_tx.clone());
    spawn_line_reader(stderr, "stderr", line_tx);

    let mut stdout_tail: Vec<String> = Vec::new();
    let mut stderr_tail: Vec<String> = Vec::new();
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(timeout));
    tokio::pin!(deadline);

    let mut cancelled = false;
    let mut timed_out = false;
    let exit_code;

    loop {
        tokio::select! {
            // Fair (unbiased) so a command streaming output non-stop can't
            // starve the cancel/timeout branches. On the normal-exit path
            // the reader drains all buffered lines before closing, so no
            // output is lost; tail loss only happens on cancel/timeout,
            // which are error paths that don't return a tail anyway.
            maybe_line = line_rx.recv() => {
                match maybe_line {
                    Some((stream, line)) => {
                        ctx.emit(
                            "terminal_output",
                            json!({"corr_id": ctx.corr_id, "stream": stream, "line": line}),
                        )
                        .await;
                        push_tail(if stream == "stdout" { &mut stdout_tail } else { &mut stderr_tail }, line);
                    }
                    None => {
                        // Both readers done; the child has closed its pipes.
                        // Reap it for the exit status.
                        exit_code = wait_code(&mut child).await;
                        break;
                    }
                }
            }
            _ = ctx.cancel.cancelled() => {
                let _ = child.start_kill();
                cancelled = true;
                exit_code = wait_code(&mut child).await;
                break;
            }
            _ = &mut deadline => {
                let _ = child.start_kill();
                timed_out = true;
                exit_code = wait_code(&mut child).await;
                break;
            }
        }
    }

    let outcome = RunOutcome {
        exit_code,
        stdout_tail,
        stderr_tail,
        duration_ms: started.elapsed().as_millis(),
        cancelled,
        timed_out,
    };

    if outcome.cancelled {
        return Err("cancelled".to_string());
    }
    if outcome.timed_out {
        return Err(format!("timed out after {timeout}s"));
    }
    Ok(json!({
        "exit_code": outcome.exit_code,
        "stdout": outcome.stdout_tail.join("\n"),
        "stderr": outcome.stderr_tail.join("\n"),
        "duration_ms": outcome.duration_ms,
    }))
}

/// Read `reader` line-by-line, forwarding each into `tx` tagged with its
/// stream name. Ends when the pipe closes.
fn spawn_line_reader<R>(reader: R, stream: &'static str, tx: mpsc::Sender<(&'static str, String)>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send((stream, line)).await.is_err() {
                break;
            }
        }
    });
}

/// Await the child's exit and return its code (`None` if killed by signal).
async fn wait_code(child: &mut Child) -> Option<i32> {
    match child.wait().await {
        Ok(status) => status.code(),
        Err(_) => None,
    }
}

/// Push a line onto a bounded tail, dropping the oldest when full.
fn push_tail(tail: &mut Vec<String>, line: String) {
    if tail.len() == TAIL_LINES {
        tail.remove(0);
    }
    tail.push(line);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::executor::tests_support::{collecting_ctx, drain};
    use peckboard_agent_protocol::AgentFrame;
    use std::time::Duration;

    #[tokio::test]
    async fn echoes_and_reports_zero_exit() {
        let (ctx, mut events) = collecting_ctx();
        let out = TerminalExecutor
            .execute(&ctx, json!({"command": "printf 'hello\\nworld\\n'"}))
            .await
            .unwrap();
        assert_eq!(out["exit_code"], 0);
        assert_eq!(out["stdout"], "hello\nworld");
        // Two streamed terminal_output events (one per line).
        let lines: Vec<String> = drain(&mut events)
            .iter()
            .filter_map(|f| match f {
                AgentFrame::Event { kind, data } if kind == "terminal_output" => {
                    Some(data["line"].as_str().unwrap().to_string())
                }
                _ => None,
            })
            .collect();
        assert_eq!(lines, vec!["hello".to_string(), "world".to_string()]);
    }
    #[tokio::test]
    async fn nonzero_exit_is_reported_not_errored() {
        let (ctx, _events) = collecting_ctx();
        let out = TerminalExecutor
            .execute(&ctx, json!({"command": "exit 3"}))
            .await
            .unwrap();
        assert_eq!(out["exit_code"], 3);
    }

    #[tokio::test]
    async fn stderr_is_captured() {
        let (ctx, _events) = collecting_ctx();
        let out = TerminalExecutor
            .execute(&ctx, json!({"command": "echo oops 1>&2"}))
            .await
            .unwrap();
        assert_eq!(out["stderr"], "oops");
    }

    #[tokio::test]
    async fn cancel_kills_the_child() {
        let (ctx, _events) = collecting_ctx();
        let cancel = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();
        });
        let err = TerminalExecutor
            .execute(&ctx, json!({"command": "sleep 30"}))
            .await
            .unwrap_err();
        assert_eq!(err, "cancelled");
    }

    #[tokio::test]
    async fn timeout_kills_the_child() {
        let (ctx, _events) = collecting_ctx();
        let err = TerminalExecutor
            .execute(&ctx, json!({"command": "sleep 30", "timeout_secs": 1}))
            .await
            .unwrap_err();
        assert!(err.contains("timed out"), "got: {err}");
    }

    #[tokio::test]
    async fn empty_command_rejected() {
        let (ctx, _events) = collecting_ctx();
        let err = TerminalExecutor
            .execute(&ctx, json!({"command": "  "}))
            .await
            .unwrap_err();
        assert!(err.contains("must not be empty"), "got: {err}");
    }
}
