//! Grok CLI (`grok`) agent provider.
//!
//! Drives sessions through Grok's headless mode. Like the Cursor provider —
//! and unlike Claude's long-lived duplex child — `grok` is invoked **once
//! per turn** in single-prompt streaming mode:
//!
//! ```text
//! grok --single=<prompt> --output-format=streaming-json \
//!     [--model=M] [--session-id=SESS] [--effort=LEVEL] --always-approve
//! ```
//!
//! Each invocation streams newline-delimited JSON (`text` / `thought` /
//! `tool_call` / `tool` / `end`) which [`parser`] turns into the unified
//! [`ProviderEvent`] stream. Grok's `sessionId` is captured from the `end`
//! frame and emitted on `Completed` so the next turn can resume the same
//! conversation with `--session-id`.
//!
//! Multi-account works exactly like the Claude provider: a session's model id
//! may carry an `@<account_id>` suffix, which resolves to a per-account
//! `GROK_HOME` (and, for `api_key` accounts, an `XAI_API_KEY`) injected at
//! spawn time. A bare model id uses the host's ambient `~/.grok` credentials.
//!
//! Because each turn is its own short-lived process, the provider keeps the
//! default `supports_mid_stream_injection() == false`: the SessionManager
//! queues a second message and drains it when the current turn completes.

pub mod login;
mod parser;

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

use crate::provider::agent::{AgentProvider, ProcessCompletion, SendMessageContext, emit_event};
use crate::provider::registry::split_model_account;
use crate::provider::stream::{ModelInfo, ProviderEvent};

/// Default per-turn timeout. Grok turns can take several tool steps, so this
/// is generous. An unauthenticated account is caught far sooner (the device
/// prompt on stderr fast-fails the turn), so this only bounds genuine work.
const DEFAULT_TIMEOUT_SECS: u64 = 600;
/// CLI binary name.
const DEFAULT_CLI: &str = "grok";
/// Cap on stderr bytes captured for a crash message.
const MAX_STDERR_BYTES: usize = 16 * 1024;
/// Substring of the device-login URL grok prints to stderr when the account
/// it's running as isn't signed in. Seeing it means the turn would otherwise
/// block forever on "Waiting for authorization...", so we fast-fail.
const DEVICE_LOGIN_MARKER: &str = "accounts.x.ai/oauth2/device";

/// Per-session tracking for an in-flight `grok` turn.
struct GrokRun {
    handle: JoinHandle<()>,
    cancel: Arc<Notify>,
}

/// `AgentProvider` backed by per-turn `grok` invocations.
pub struct GrokProvider {
    runs: Arc<Mutex<HashMap<String, GrokRun>>>,
    /// DB handle for multi-account support: `dynamic_models` enumerates the
    /// stored accounts and `send_message` resolves the per-account credential
    /// to inject. `None` in tests / no-DB registrations keeps the
    /// single-(Default-)account behaviour.
    db: Option<crate::db::Db>,
}

impl GrokProvider {
    pub fn new() -> Self {
        GrokProvider {
            runs: Arc::new(Mutex::new(HashMap::new())),
            db: None,
        }
    }

    /// Attach a DB handle so the provider can resolve Grok accounts.
    pub fn with_db(mut self, db: crate::db::Db) -> Self {
        self.db = Some(db);
        self
    }

    /// Resolve `account_id` to its credential and add the env the spawned
    /// `grok` CLI needs to run as that account: every account gets an isolated
    /// `GROK_HOME` (its `config_dir`), and an `api_key` account additionally
    /// injects `XAI_API_KEY`. An account id that no longer exists (deleted out
    /// from under a live session) is a hard error rather than a silent fall
    /// back to host credentials — a turn must never bill the wrong account.
    async fn inject_account_env(
        &self,
        account_id: &str,
        env: &mut HashMap<String, String>,
    ) -> anyhow::Result<()> {
        let Some(db) = &self.db else {
            return Ok(());
        };
        let account = db
            .get_grok_account(account_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("grok account not found: {account_id}"))?;
        if let Some(dir) = &account.config_dir {
            std::fs::create_dir_all(dir).ok();
            env.insert("GROK_HOME".into(), dir.clone());
        }
        if account.kind == "api_key" {
            env.insert("XAI_API_KEY".into(), account.credential.clone());
        }
        Ok(())
    }

    /// The model catalog the picker shows: the Default-account models (bare
    /// ids) plus one labelled variant per stored account
    /// (`<model>@<account_id>`, shown as `[Account] Model`). Mirrors the
    /// Claude provider's `account_scoped_models`.
    async fn account_scoped_models(&self) -> Vec<ModelInfo> {
        let base = default_models();
        let Some(db) = &self.db else {
            return base;
        };
        let accounts = match db.list_grok_accounts().await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!("grok: failed to list accounts for model catalog: {e}");
                return base;
            }
        };
        if accounts.is_empty() {
            return base;
        }
        let mut out = base.clone();
        for acct in &accounts {
            for m in &base {
                out.push(ModelInfo {
                    id: format!("{}@{}", m.id, acct.id),
                    display_name: format!("[{}] {}", acct.name, m.display_name),
                    capabilities: m.capabilities.clone(),
                });
            }
        }
        out
    }
}

impl Default for GrokProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentProvider for GrokProvider {
    fn id(&self) -> &str {
        "grok"
    }

    async fn dynamic_models(&self) -> Option<Vec<ModelInfo>> {
        Some(self.account_scoped_models().await)
    }

    async fn send_message(&self, ctx: SendMessageContext) -> anyhow::Result<()> {
        let SendMessageContext {
            session_id,
            message,
            db,
            broadcaster,
            config,
            conversation_id,
            completion_tx,
            // grok runs its own tool loop; the plugin host (MCP tool
            // execution) isn't wired in for v1.
            plugins: _,
        } = ctx;

        // Wind down any prior run on this session before starting a new one.
        {
            let mut runs = self.runs.lock().await;
            if let Some(old) = runs.remove(&session_id) {
                old.cancel.notify_one();
            }
        }

        // Strip the `grok:` prefix, then peel off any `@<account_id>` suffix
        // and resolve it to the credential env. A model with no suffix is the
        // implicit Default account: nothing injected, host credentials apply.
        let stripped = config
            .model
            .strip_prefix("grok:")
            .map(|m| m.to_string())
            .unwrap_or_else(|| config.model.clone());
        let (base_model, account_id) = split_model_account(&stripped);
        let model = if base_model.is_empty() {
            DEFAULT_MODEL.to_string()
        } else {
            base_model.to_string()
        };

        let mut env = config.env.clone();
        if let Some(account_id) = account_id {
            self.inject_account_env(account_id, &mut env).await?;
        }

        if !message.attachments.is_empty() {
            tracing::warn!(
                session_id = %session_id,
                "grok: dropping {} attachment(s) — the grok provider is text-only for now",
                message.attachments.len()
            );
        }

        let args = build_cli_args(
            &model,
            &message.text,
            conversation_id.as_deref(),
            config.effort.as_deref(),
            config.system_prompt_override.as_deref(),
        );

        let cancel = Arc::new(Notify::new());
        let cancel_for_task = cancel.clone();
        let runs = self.runs.clone();
        let sid = session_id.clone();
        let model_label = config.model.clone();
        let working_dir = config.working_dir.clone();

        let handle = tokio::spawn(async move {
            let completed = run_turn(TurnArgs {
                args: &args,
                env: &env,
                working_dir: &working_dir,
                model_label: &model_label,
                session_id: &sid,
                db: &db,
                broadcaster: broadcaster.as_ref(),
                timeout_secs: DEFAULT_TIMEOUT_SECS,
                cancel: cancel_for_task,
            })
            .await;

            runs.lock().await.remove(&sid);

            let _ = completion_tx
                .send(ProcessCompletion {
                    session_id: sid,
                    completed,
                })
                .await;
        });

        self.runs
            .lock()
            .await
            .insert(session_id, GrokRun { handle, cancel });
        Ok(())
    }

    async fn cancel(&self, session_id: &str) {
        let cancel = {
            let runs = self.runs.lock().await;
            runs.get(session_id).map(|r| r.cancel.clone())
        };
        if let Some(c) = cancel {
            tracing::info!(session_id = %session_id, "Cancelling grok run");
            c.notify_one();
        }
    }

    async fn interrupt(&self, session_id: &str) {
        self.cancel(session_id).await;
    }

    async fn write_stdin(&self, _session_id: &str, _text: &str) -> bool {
        // Per-turn invocation has no persistent stdin: every message arrives
        // through send_message as a fresh turn.
        false
    }

    async fn is_running(&self, session_id: &str) -> bool {
        let runs = self.runs.lock().await;
        runs.get(session_id)
            .map(|r| !r.handle.is_finished())
            .unwrap_or(false)
    }

    async fn wait_for_termination(&self, session_id: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if !self.runs.lock().await.contains_key(session_id) {
                return;
            }
            if Instant::now() >= deadline {
                tracing::warn!(
                    session_id = %session_id,
                    "wait_for_termination timed out for grok run"
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn cleanup(&self) {
        let mut runs = self.runs.lock().await;
        runs.retain(|_, r| !r.handle.is_finished());
    }

    async fn shutdown(&self) {
        let mut runs = self.runs.lock().await;
        for (_, run) in runs.drain() {
            run.cancel.notify_one();
            run.handle.abort();
        }
    }
}

/// Arguments for a single `grok` turn.
struct TurnArgs<'a> {
    args: &'a [String],
    env: &'a HashMap<String, String>,
    working_dir: &'a str,
    model_label: &'a str,
    session_id: &'a str,
    db: &'a crate::db::Db,
    broadcaster: &'a crate::ws::broadcaster::Broadcaster,
    timeout_secs: u64,
    cancel: Arc<Notify>,
}

/// Spawn `grok` for one turn, stream its stdout into provider events, and emit
/// a terminal `Completed` / `Crashed`. Returns `true` on a clean completion,
/// `false` on cancel / timeout / auth-required / spawn or runtime error.
async fn run_turn(args: TurnArgs<'_>) -> bool {
    let TurnArgs {
        args: cli_args,
        env,
        working_dir,
        model_label,
        session_id,
        db,
        broadcaster,
        timeout_secs,
        cancel,
    } = args;

    tracing::info!(
        session_id = %session_id,
        "Spawning grok: {} {}",
        DEFAULT_CLI,
        cli_args.join(" ")
    );

    let mut cmd = Command::new(DEFAULT_CLI);
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
            emit_started(db, broadcaster, session_id, model_label).await;
            crash(
                db,
                broadcaster,
                session_id,
                &format!("failed to spawn '{DEFAULT_CLI}': {e}"),
                None,
            )
            .await;
            return false;
        }
    };

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take();

    // Drain stderr concurrently: accumulate a bounded buffer for crash
    // reporting AND fire `auth_needed` if grok prints its device-login prompt
    // (which means the account isn't signed in and the turn would otherwise
    // hang on "Waiting for authorization...").
    let auth_needed = Arc::new(Notify::new());
    let auth_needed_setter = auth_needed.clone();
    let stderr_task = stderr.map(|s| {
        tokio::spawn(async move {
            let mut buf = String::new();
            let mut saw_auth = false;
            let mut lines = BufReader::new(s).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if !saw_auth && line.contains(DEVICE_LOGIN_MARKER) {
                    saw_auth = true;
                    auth_needed_setter.notify_one();
                }
                if buf.len() < MAX_STDERR_BYTES {
                    if !buf.is_empty() {
                        buf.push('\n');
                    }
                    buf.push_str(&line);
                }
            }
            (buf.trim().to_string(), saw_auth)
        })
    });

    // Emit Started up front so the UI shows an agent-start; grok streaming
    // has no init frame to derive one from.
    emit_started(db, broadcaster, session_id, model_label).await;

    let mut conversation_id: Option<String> = None;
    let mut error_reason: Option<String> = None;

    let mut lines = BufReader::new(stdout).lines();
    let deadline = tokio::time::sleep(Duration::from_secs(timeout_secs));
    tokio::pin!(deadline);

    let outcome = loop {
        tokio::select! {
            _ = cancel.notified() => {
                let _ = child.start_kill();
                break TurnOutcome::Cancelled;
            }
            _ = auth_needed.notified() => {
                let _ = child.start_kill();
                break TurnOutcome::AuthRequired;
            }
            _ = &mut deadline => {
                let _ = child.start_kill();
                break TurnOutcome::Timeout;
            }
            line = lines.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                            tracing::debug!(session_id = %session_id, "grok: non-JSON stdout line ignored");
                            continue;
                        };
                        if let Some(reason) = parser::error_reason(&json) {
                            // Remember the last error; grok still emits an
                            // `end` frame, so let the stream finish naturally.
                            error_reason = Some(reason);
                            continue;
                        }
                        for event in parser::parse_stream_json(&json, &mut conversation_id) {
                            emit_event(db, broadcaster, session_id, event).await;
                        }
                    }
                    Ok(None) => break TurnOutcome::Eof,
                    Err(e) => {
                        let _ = child.start_kill();
                        break TurnOutcome::ReadError(e.to_string());
                    }
                }
            }
        }
    };

    let status = child.wait().await.ok();
    let (stderr_text, _saw_auth) = match stderr_task {
        Some(t) => t.await.unwrap_or_default(),
        None => (String::new(), false),
    };

    match outcome {
        TurnOutcome::Eof => {
            let ok = status.map(|s| s.success()).unwrap_or(false);
            if let Some(reason) = error_reason {
                crash(db, broadcaster, session_id, &reason, exit_code(status)).await;
                false
            } else if ok {
                emit_event(
                    db,
                    broadcaster,
                    session_id,
                    ProviderEvent::Completed { conversation_id },
                )
                .await;
                true
            } else {
                let reason = if stderr_text.is_empty() {
                    "grok exited without a successful result".to_string()
                } else {
                    stderr_text
                };
                crash(db, broadcaster, session_id, &reason, exit_code(status)).await;
                false
            }
        }
        TurnOutcome::AuthRequired => {
            crash(
                db,
                broadcaster,
                session_id,
                "This Grok account isn't signed in. Open Settings → Grok accounts and \
                 complete the browser sign-in, then try again.",
                None,
            )
            .await;
            false
        }
        TurnOutcome::Cancelled => {
            // The interrupt route appends its own `interrupt` event; emit a
            // Completed so any in-flight tool spinner closes.
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::Completed { conversation_id },
            )
            .await;
            false
        }
        TurnOutcome::Timeout => {
            crash(
                db,
                broadcaster,
                session_id,
                &format!("grok turn exceeded {timeout_secs}s timeout"),
                None,
            )
            .await;
            false
        }
        TurnOutcome::ReadError(e) => {
            crash(
                db,
                broadcaster,
                session_id,
                &format!("stdout read error: {e}"),
                None,
            )
            .await;
            false
        }
    }
}

enum TurnOutcome {
    Eof,
    Cancelled,
    AuthRequired,
    Timeout,
    ReadError(String),
}

fn exit_code(status: Option<std::process::ExitStatus>) -> Option<i32> {
    status.and_then(|s| s.code())
}

async fn emit_started(
    db: &crate::db::Db,
    broadcaster: &crate::ws::broadcaster::Broadcaster,
    session_id: &str,
    model_label: &str,
) {
    emit_event(
        db,
        broadcaster,
        session_id,
        ProviderEvent::Started {
            model: model_label.to_string(),
            conversation_id: None,
            metadata: serde_json::json!({ "provider": "grok" }),
        },
    )
    .await;
}

async fn crash(
    db: &crate::db::Db,
    broadcaster: &crate::ws::broadcaster::Broadcaster,
    session_id: &str,
    reason: &str,
    exit_code: Option<i32>,
) {
    emit_event(
        db,
        broadcaster,
        session_id,
        ProviderEvent::Crashed {
            reason: reason.to_string(),
            exit_code,
            stderr: None,
        },
    )
    .await;
}

/// Build the `grok` argument vector for one turn.
///
/// Every flag uses the `--flag=VALUE` joined form so a user-controlled value
/// (the prompt above all) can never be parsed as a separate flag — the same
/// injection hardening the Claude provider applies to its argv.
fn build_cli_args(
    model: &str,
    prompt: &str,
    conversation_id: Option<&str>,
    effort: Option<&str>,
    system_prompt_override: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        format!("--single={prompt}"),
        "--output-format=streaming-json".to_string(),
        // Headless turns auto-approve tool actions; peckboard scopes work to
        // the session's working dir, matching the Cursor provider's default.
        "--always-approve".to_string(),
    ];
    if !model.is_empty() {
        args.push(format!("--model={model}"));
    }
    if let Some(cid) = conversation_id {
        // `--session-id` creates the session if absent or resumes it if it
        // exists, so the same flag covers first-turn and resume.
        args.push(format!("--session-id={cid}"));
    }
    if let Some(effort) = effort.map(str::trim).filter(|e| !e.is_empty()) {
        args.push(format!("--effort={effort}"));
    }
    if let Some(sp) = system_prompt_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        args.push(format!("--system-prompt-override={sp}"));
    }
    args
}

/// Grok's default (and, when unauthenticated, only visible) model.
const DEFAULT_MODEL: &str = "grok-build";

/// The built-in seed model list. Grok exposes its full catalog only to an
/// authenticated account; the picker seeds the verified default and the
/// per-account variants are layered on by `account_scoped_models`.
pub fn default_models() -> Vec<ModelInfo> {
    vec![ModelInfo {
        id: DEFAULT_MODEL.into(),
        display_name: "Grok Build".into(),
        capabilities: vec!["code".into(), "reasoning".into()],
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_args_hardens_prompt_and_sets_streaming() {
        let args = build_cli_args("grok-build", "--always-approve evil", None, None, None);
        // The whole prompt is the value of --single, never a separate flag.
        assert_eq!(args[0], "--single=--always-approve evil");
        assert!(args.contains(&"--output-format=streaming-json".to_string()));
        assert!(args.contains(&"--always-approve".to_string()));
        assert!(args.contains(&"--model=grok-build".to_string()));
        // No bare `--always-approve evil` token splitting out of the prompt.
        assert!(!args.iter().any(|a| a == "evil"));
    }

    #[test]
    fn build_args_includes_session_effort_and_system_prompt() {
        let args = build_cli_args(
            "grok-build",
            "hi",
            Some("sess-7"),
            Some("high"),
            Some("be terse"),
        );
        assert!(args.contains(&"--session-id=sess-7".to_string()));
        assert!(args.contains(&"--effort=high".to_string()));
        assert!(args.contains(&"--system-prompt-override=be terse".to_string()));
    }

    #[test]
    fn build_args_omits_optional_flags_when_absent() {
        let args = build_cli_args("grok-build", "hi", None, Some("  "), None);
        assert!(!args.iter().any(|a| a.starts_with("--session-id")));
        // Whitespace-only effort is treated as absent.
        assert!(!args.iter().any(|a| a.starts_with("--effort")));
        assert!(
            !args
                .iter()
                .any(|a| a.starts_with("--system-prompt-override"))
        );
    }

    #[test]
    fn default_models_are_prefix_free() {
        for m in default_models() {
            assert!(!m.id.contains(':'), "id {} should be prefix-free", m.id);
            assert!(!m.id.contains('@'), "id {} should be account-free", m.id);
        }
    }
}
