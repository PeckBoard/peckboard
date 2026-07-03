//! Cursor CLI (`cursor-agent`) agent provider.
//!
//! Drives sessions through Cursor's headless CLI. Unlike the Claude
//! provider — which owns one long-lived duplex child per session —
//! `cursor-agent` is invoked **once per turn** in print mode:
//!
//! ```text
//! cursor-agent --print --output-format stream-json [--model M] \
//!     [--resume CHAT_ID] [--force] -- "<prompt>"
//! ```
//!
//! Each invocation streams newline-delimited JSON (Claude Code-compatible:
//! `system`/`assistant`/`user`/`result`) which [`parser`] turns into the
//! unified [`ProviderEvent`] stream. Cursor's `session_id` (chat id) is
//! captured from the stream and emitted on `Completed` so the next turn can
//! `--resume` the same conversation.
//!
//! Because each turn is its own short-lived process, the provider keeps the
//! default `supports_mid_stream_injection() == false`: the SessionManager
//! queues a second message and drains it when the current turn completes.
//!
//! NOTE: `cursor-agent` is an external CLI whose flags and stream-json
//! schema aren't formally specified. The invocation above and the parser
//! are written defensively and the CLI path / flags are configurable via
//! plugin settings; validate against your installed `cursor-agent` version.

mod parser;

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

use crate::plugin::settings::PluginSettingsStore;
use crate::provider::agent::{AgentProvider, ProcessCompletion, SendMessageContext, emit_event};
use crate::provider::stream::{ModelInfo, ProviderEvent};

/// Default per-turn timeout. Cursor agent turns can run for a while when
/// the model takes several tool steps, so this is generous.
const DEFAULT_TIMEOUT_SECS: u64 = 600;
/// Hard ceiling so a misconfigured setting can't wedge a worker forever.
const MAX_TIMEOUT_SECS: u64 = 3600;
/// Default CLI binary name; overridable via the `cli_path` setting.
const DEFAULT_CLI: &str = "cursor-agent";
/// How long a model-discovery probe (success or failure) is cached, so the
/// picker doesn't shell out on every render.
const MODEL_DISCOVERY_TTL: Duration = Duration::from_secs(60);
/// Bound on how long the discovery subprocess may run.
const MODEL_DISCOVERY_TIMEOUT_SECS: u64 = 10;
/// Cap on stderr bytes captured for a crash message.
const MAX_STDERR_BYTES: usize = 16 * 1024;

/// Per-session tracking for an in-flight `cursor-agent` turn.
struct CursorRun {
    handle: JoinHandle<()>,
    cancel: Arc<Notify>,
}

/// TTL cache for the model-discovery probe.
struct DiscoveryCache {
    fetched_at: Instant,
    models: Option<Vec<String>>,
}

/// `AgentProvider` backed by per-turn `cursor-agent` invocations.
pub struct CursorProvider {
    settings: PluginSettingsStore,
    runs: Arc<Mutex<HashMap<String, CursorRun>>>,
    discovery_cache: Arc<Mutex<Option<DiscoveryCache>>>,
}

impl CursorProvider {
    pub fn new(settings: PluginSettingsStore) -> Self {
        CursorProvider {
            settings,
            runs: Arc::new(Mutex::new(HashMap::new())),
            discovery_cache: Arc::new(Mutex::new(None)),
        }
    }

    /// Run the discovery command and return model ids, going through the TTL
    /// cache. `Some(list)` on success (possibly empty), `None` when the last
    /// probe failed and the caller should fall back to the static seed.
    async fn discovered_models(&self, cli_path: &str) -> Option<Vec<String>> {
        {
            let cache = self.discovery_cache.lock().await;
            if let Some(entry) = cache.as_ref()
                && entry.fetched_at.elapsed() < MODEL_DISCOVERY_TTL
            {
                return entry.models.clone();
            }
        }
        let result = probe_cli_models(cli_path).await;
        let mut cache = self.discovery_cache.lock().await;
        *cache = Some(DiscoveryCache {
            fetched_at: Instant::now(),
            models: result.clone(),
        });
        result
    }
}

#[async_trait]
impl AgentProvider for CursorProvider {
    fn id(&self) -> &str {
        "cursor"
    }

    async fn dynamic_models(&self) -> Option<Vec<ModelInfo>> {
        let settings = match self.settings.load().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("cursor: failed to load settings for model list: {e}");
                HashMap::new()
            }
        };
        let cli_path =
            setting_str(&settings, "cli_path").unwrap_or_else(|| DEFAULT_CLI.to_string());
        let extras = setting_str_list(&settings, "additional_models");
        let discover = setting_bool(&settings, "discover_models").unwrap_or(true);

        let base = if discover {
            match self.discovered_models(&cli_path).await {
                Some(ids) if !ids.is_empty() => ids.into_iter().map(model_info).collect(),
                // Discovery failed or returned nothing usable → static seed.
                _ => default_models(),
            }
        } else {
            default_models()
        };

        Some(merge_additional_models(base, extras))
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
            // cursor-agent runs its own tool loop; the plugin host (MCP
            // tool execution) isn't wired in for v1.
            plugins: _,
        } = ctx;

        // Wind down any prior run on this session before starting a new one.
        {
            let mut runs = self.runs.lock().await;
            if let Some(old) = runs.remove(&session_id) {
                old.cancel.notify_one();
            }
        }

        let settings = self.settings.load().await?;
        let cli_path =
            setting_str(&settings, "cli_path").unwrap_or_else(|| DEFAULT_CLI.to_string());
        let default_model = setting_str(&settings, "default_model");
        let timeout_secs = setting_int(&settings, "request_timeout_secs")
            .map(|n| n.max(1) as u64)
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS);
        let auto_approve = setting_bool(&settings, "auto_approve").unwrap_or(true);

        let model = resolve_model(&config.model)
            .or(default_model)
            .unwrap_or_else(|| "auto".to_string());

        if !message.attachments.is_empty() {
            tracing::warn!(
                session_id = %session_id,
                "cursor: dropping {} attachment(s) — the cursor-agent provider is text-only for now",
                message.attachments.len()
            );
        }

        // cursor has no override concept plumbed, so the shared working-style
        // rules are the system prompt (used only on the first turn).
        let args = build_cli_args(
            &model,
            &message.text,
            conversation_id.as_deref(),
            auto_approve,
            crate::provider::WORKING_STYLE,
        );

        let cancel = Arc::new(Notify::new());
        let cancel_for_task = cancel.clone();
        let runs = self.runs.clone();
        let sid = session_id.clone();
        let model_label = config.model.clone();
        let working_dir = config.working_dir.clone();

        let handle = tokio::spawn(async move {
            let completed = run_turn(TurnArgs {
                cli_path: &cli_path,
                args: &args,
                working_dir: &working_dir,
                model_label: &model_label,
                session_id: &sid,
                db: &db,
                broadcaster: broadcaster.as_ref(),
                timeout_secs,
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
            .insert(session_id, CursorRun { handle, cancel });
        Ok(())
    }

    async fn cancel(&self, session_id: &str) {
        let cancel = {
            let runs = self.runs.lock().await;
            runs.get(session_id).map(|r| r.cancel.clone())
        };
        if let Some(c) = cancel {
            tracing::info!(session_id = %session_id, "Cancelling cursor run");
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
                    "wait_for_termination timed out for cursor run"
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

/// Arguments for a single `cursor-agent` turn.
struct TurnArgs<'a> {
    cli_path: &'a str,
    args: &'a [String],
    working_dir: &'a str,
    model_label: &'a str,
    session_id: &'a str,
    db: &'a crate::db::Db,
    broadcaster: &'a crate::ws::broadcaster::Broadcaster,
    timeout_secs: u64,
    cancel: Arc<Notify>,
}

/// Spawn `cursor-agent` for one turn, stream its stdout into provider
/// events, and emit a terminal `Completed` / `Crashed`. Returns `true` on a
/// clean completion, `false` on cancel / timeout / spawn or runtime error.
async fn run_turn(args: TurnArgs<'_>) -> bool {
    let TurnArgs {
        cli_path,
        args: cli_args,
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
        "Spawning cursor-agent: {} {}",
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

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            // Surface a Started→Crashed pair so the UI shows the failure
            // rather than a silent no-op.
            emit_started(db, broadcaster, session_id, model_label).await;
            crash(
                db,
                broadcaster,
                session_id,
                &format!("failed to spawn '{cli_path}': {e}"),
                None,
            )
            .await;
            return false;
        }
    };

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take();
    // Drain stderr concurrently into a bounded buffer for crash reporting.
    let stderr_task = stderr.map(|mut s| {
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while let Ok(n) = s.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                if buf.len() < MAX_STDERR_BYTES {
                    buf.extend_from_slice(&chunk[..n.min(MAX_STDERR_BYTES - buf.len())]);
                }
            }
            String::from_utf8_lossy(&buf).trim().to_string()
        })
    });

    let mut conversation_id: Option<String> = None;
    let mut model_name: Option<String> = None;
    let mut emitted_start = false;
    let mut saw_any = false;

    let mut lines = BufReader::new(stdout).lines();
    let deadline = tokio::time::sleep(Duration::from_secs(timeout_secs));
    tokio::pin!(deadline);

    let outcome = loop {
        tokio::select! {
            _ = cancel.notified() => {
                let _ = child.start_kill();
                break TurnOutcome::Cancelled;
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
                            // Non-JSON noise on stdout — log and skip.
                            tracing::debug!(session_id = %session_id, "cursor: non-JSON stdout line ignored");
                            continue;
                        };
                        let events = parser::parse_stream_json(
                            &json,
                            &mut conversation_id,
                            &mut model_name,
                            &mut emitted_start,
                        );
                        for event in events {
                            saw_any = true;
                            emit_event(db, broadcaster, session_id, event).await;
                        }
                    }
                    Ok(None) => break TurnOutcome::Eof,
                    Err(e) => {
                        tracing::warn!(session_id = %session_id, "cursor: stdout read error: {e}");
                        let _ = child.start_kill();
                        break TurnOutcome::ReadError(e.to_string());
                    }
                }
            }
        }
    };

    // Let the child fully exit and collect its status + stderr.
    let status = child.wait().await.ok();
    let stderr_text = match stderr_task {
        Some(t) => t.await.unwrap_or_default(),
        None => String::new(),
    };

    match outcome {
        TurnOutcome::Eof => {
            let ok = status.map(|s| s.success()).unwrap_or(false);
            if ok || saw_any {
                if !emitted_start {
                    emit_started(db, broadcaster, session_id, model_label).await;
                }
                emit_event(
                    db,
                    broadcaster,
                    session_id,
                    ProviderEvent::Completed { conversation_id },
                )
                .await;
                true
            } else {
                if !emitted_start {
                    emit_started(db, broadcaster, session_id, model_label).await;
                }
                let reason = if stderr_text.is_empty() {
                    "cursor-agent exited without output".to_string()
                } else {
                    stderr_text.clone()
                };
                crash(db, broadcaster, session_id, &reason, exit_code(status)).await;
                false
            }
        }
        TurnOutcome::Cancelled => {
            // The interrupt route appends its own `interrupt` event; emit a
            // Completed so any in-flight tool spinner closes and the
            // orchestrator sees a clean end.
            if !emitted_start {
                emit_started(db, broadcaster, session_id, model_label).await;
            }
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
            if !emitted_start {
                emit_started(db, broadcaster, session_id, model_label).await;
            }
            crash(
                db,
                broadcaster,
                session_id,
                &format!("cursor-agent turn exceeded {timeout_secs}s timeout"),
                None,
            )
            .await;
            false
        }
        TurnOutcome::ReadError(e) => {
            if !emitted_start {
                emit_started(db, broadcaster, session_id, model_label).await;
            }
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
            metadata: serde_json::json!({ "provider": "cursor" }),
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

/// Build the `cursor-agent` argument vector for one turn.
///
/// The prompt is passed as a positional argument after `--` so a prompt
/// that begins with `-` isn't mistaken for a flag. `auto` (or an empty
/// model) means "let Cursor choose", so `--model` is omitted.
fn build_cli_args(
    model: &str,
    prompt: &str,
    conversation_id: Option<&str>,
    auto_approve: bool,
    system_prompt: &str,
) -> Vec<String> {
    let mut args = vec![
        "--print".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
    ];
    if !model.is_empty() && model != "auto" {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if let Some(cid) = conversation_id {
        args.push("--resume".to_string());
        args.push(cid.to_string());
    }
    if auto_approve {
        // Non-interactive auto-approval of tool actions in headless mode.
        args.push("--force".to_string());
    }
    args.push("--".to_string());
    // `cursor-agent` has no system-prompt / rules flag (its rules are an
    // interactive `generate-rule` flow), so the shared working-style rules
    // are folded into the prompt. Only on the FIRST turn of a conversation
    // (no `--resume` id) — resumes carry the model's context forward, so
    // repeating the rules every turn would just waste tokens.
    let prompt = match conversation_id {
        None if !system_prompt.trim().is_empty() => {
            format!("{}\n\n{}", system_prompt.trim(), prompt)
        }
        _ => prompt.to_string(),
    };
    args.push(prompt);
    args
}

/// Run the discovery command (`cursor-agent models --output-format json`)
/// and parse the model ids. `None` on any failure so the caller seeds
/// statically.
async fn probe_cli_models(cli_path: &str) -> Option<Vec<String>> {
    let mut cmd = Command::new(cli_path);
    cmd.args(["models", "--output-format", "json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("cursor: model discovery spawn failed: {e}");
            return None;
        }
    };

    let output = match tokio::time::timeout(
        Duration::from_secs(MODEL_DISCOVERY_TIMEOUT_SECS),
        child.wait_with_output(),
    )
    .await
    {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            tracing::warn!("cursor: model discovery failed: {e}");
            return None;
        }
        Err(_) => {
            tracing::warn!("cursor: model discovery timed out");
            return None;
        }
    };

    if !output.status.success() {
        tracing::warn!(
            "cursor: model discovery exited with {:?}",
            output.status.code()
        );
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parser::parse_cli_models(&text)
}

/// Strip the `cursor:` provider prefix. `None` for a bare model string or a
/// prefix that isn't ours, so the caller falls back to the configured
/// default model.
fn resolve_model(raw: &str) -> Option<String> {
    let rest = raw.strip_prefix("cursor:")?;
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    }
}

fn model_info(name: String) -> ModelInfo {
    ModelInfo {
        display_name: format!("{name} (Cursor)"),
        id: name,
        capabilities: vec!["code".into()],
    }
}

/// The built-in seed model list, used when discovery is off or fails.
pub fn default_models() -> Vec<ModelInfo> {
    // Fallback seed only — when discovery is enabled (the default) the live
    // `cursor-agent models` list supersedes this. Kept to a small set of
    // current flagships so the picker is still usable offline.
    [
        ("auto", "Auto (Cursor)"),
        ("composer-2.5", "Composer 2.5 (Cursor)"),
        ("composer-2.5-fast", "Composer 2.5 Fast (Cursor)"),
        (
            "claude-opus-4-8-thinking-high",
            "Claude Opus 4.8 Thinking (Cursor)",
        ),
        ("claude-4.5-sonnet", "Claude Sonnet 4.5 (Cursor)"),
        (
            "claude-4.5-sonnet-thinking",
            "Claude Sonnet 4.5 Thinking (Cursor)",
        ),
        ("gpt-5.5-high", "GPT-5.5 High (Cursor)"),
        ("gpt-5.3-codex", "Codex 5.3 (Cursor)"),
        ("gemini-3.1-pro", "Gemini 3.1 Pro (Cursor)"),
        ("grok-4.3", "Grok 4.3 (Cursor)"),
    ]
    .into_iter()
    .map(|(id, name)| ModelInfo {
        id: id.into(),
        display_name: name.into(),
        capabilities: vec!["code".into()],
    })
    .collect()
}

/// Append `extras` to `base`, skipping ids already present (preserving order).
fn merge_additional_models(base: Vec<ModelInfo>, extras: Vec<String>) -> Vec<ModelInfo> {
    let mut seen: std::collections::HashSet<String> = base.iter().map(|m| m.id.clone()).collect();
    let mut models = base;
    for name in extras {
        if seen.insert(name.clone()) {
            models.push(model_info(name));
        }
    }
    models
}

fn setting_str(settings: &HashMap<String, serde_json::Value>, key: &str) -> Option<String> {
    settings
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn setting_int(settings: &HashMap<String, serde_json::Value>, key: &str) -> Option<i64> {
    settings.get(key).and_then(|v| v.as_i64())
}

fn setting_bool(settings: &HashMap<String, serde_json::Value>, key: &str) -> Option<bool> {
    settings.get(key).and_then(|v| v.as_bool())
}

fn setting_str_list(settings: &HashMap<String, serde_json::Value>, key: &str) -> Vec<String> {
    let Some(arr) = settings.get(key).and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_model_strips_prefix() {
        assert_eq!(resolve_model("cursor:gpt-5"), Some("gpt-5".into()));
        assert_eq!(resolve_model("cursor:"), None);
        assert_eq!(resolve_model("gpt-5"), None);
        assert_eq!(resolve_model("claude:opus"), None);
    }

    #[test]
    fn build_args_omits_model_for_auto_and_quotes_prompt_positionally() {
        // No system prompt passed here — the positional is exactly the prompt.
        let args = build_cli_args("auto", "hello", None, true, "");
        assert!(!args.iter().any(|a| a == "--model"));
        assert!(args.contains(&"--print".to_string()));
        assert!(args.contains(&"stream-json".to_string()));
        assert!(args.contains(&"--force".to_string()));
        // Prompt is the final positional, after `--`.
        assert_eq!(args.last().unwrap(), "hello");
        let dd = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(args[dd + 1], "hello");
    }

    #[test]
    fn build_args_includes_model_and_resume() {
        let args = build_cli_args("gpt-5", "do it", Some("chat-7"), false, "");
        let m = args.iter().position(|a| a == "--model").unwrap();
        assert_eq!(args[m + 1], "gpt-5");
        let r = args.iter().position(|a| a == "--resume").unwrap();
        assert_eq!(args[r + 1], "chat-7");
        assert!(!args.iter().any(|a| a == "--force"));
    }

    #[test]
    fn first_turn_prepends_working_style_rules_but_resume_does_not() {
        // First turn (no conversation id): the rules are folded into the
        // prompt ahead of the user's text.
        let first = build_cli_args("auto", "do it", None, true, crate::provider::WORKING_STYLE);
        let prompt = first.last().unwrap();
        assert!(prompt.contains("# Working style"));
        assert!(prompt.ends_with("do it"));

        // Resume turn (conversation id present): rules are NOT repeated.
        let resume = build_cli_args(
            "auto",
            "do it",
            Some("chat-7"),
            true,
            crate::provider::WORKING_STYLE,
        );
        assert_eq!(resume.last().unwrap(), "do it");
    }

    #[test]
    fn merge_additional_models_dedups_against_seed() {
        let merged = merge_additional_models(
            default_models(),
            vec!["auto".into(), "my-custom".into(), "my-custom".into()],
        );
        let ids: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"auto"));
        assert!(ids.contains(&"my-custom"));
        // "auto" already seeded → not duplicated.
        assert_eq!(ids.iter().filter(|id| **id == "auto").count(), 1);
        assert_eq!(ids.iter().filter(|id| **id == "my-custom").count(), 1);
    }

    #[test]
    fn default_models_are_prefix_free_ids() {
        // ProviderInfo ids must be bare (the registry adds the `cursor:`
        // prefix when building full model ids).
        for m in default_models() {
            assert!(!m.id.contains(':'), "id {} should be prefix-free", m.id);
        }
    }
}
