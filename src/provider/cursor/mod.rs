//! Cursor CLI (`cursor-agent`) agent provider.
//!
//! Drives sessions through Cursor's headless CLI. Unlike the Claude
//! provider — which owns one long-lived duplex child per session —
//! `cursor-agent` is invoked **once per turn** in print mode:
//!
//! ```text
//! cursor-agent --print --output-format stream-json --stream-partial-output \
//!     [--model M] [--resume CHAT_ID] [--force --trust] -- "<prompt>"
//! ```
//!
//! Each invocation streams newline-delimited JSON (`system` init, streamed
//! `assistant` text deltas, `tool_call` started/completed, `result` — see
//! [`parser`] docs) which [`parser`] turns into the
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

mod mcp;
mod parser;

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::process::Command;
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

use crate::plugin::settings::PluginSettingsStore;
use crate::provider::agent::{AgentProvider, ProcessCompletion, SendMessageContext};
use crate::provider::stream::{ModelInfo, ProviderEvent};
use crate::provider::turn::{
    self, TurnSpec, TurnStream, setting_bool, setting_str, setting_str_list,
};

/// Default CLI binary name; overridable via the `cli_path` setting.
const DEFAULT_CLI: &str = "cursor-agent";
/// How long a model-discovery probe (success or failure) is cached, so the
/// picker doesn't shell out on every render.
const MODEL_DISCOVERY_TTL: Duration = Duration::from_secs(60);
/// Bound on how long the discovery subprocess may run.
const MODEL_DISCOVERY_TIMEOUT_SECS: u64 = 10;
/// Wall-clock bound on a single turn. `cursor-agent` enforces no timeout of
/// its own, so without this a wedged child pins the session forever.
const DEFAULT_TIMEOUT_SECS: u64 = turn::DEFAULT_TIMEOUT_SECS;

/// Per-session tracking for an in-flight `cursor-agent` turn.
struct CursorRun {
    handle: JoinHandle<()>,
    cancel: Arc<Notify>,
    /// Graceful "your card is done, wrap up" signal — see `TurnSpec::retire`.
    retire: Arc<Notify>,
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
            run_id,
            conversation_id,
            completion_tx,
            // cursor-agent runs its own tool loop, so the plugin host isn't
            // wired as a *tool* provider; peckboard MCP tools reach it via
            // the workspace `.cursor/mcp.json` + a per-spawn token env var
            // (see `mcp`). It IS handed to the turn harness so a `todo`-hook
            // plugin can drive lifecycle tracking off the assistant text.
            plugins,
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
        let auto_approve = setting_bool(&settings, "auto_approve").unwrap_or(true);

        let model = resolve_model(&config.model)
            .or(default_model)
            .unwrap_or_else(|| "auto".to_string());

        if !message.attachments.is_empty() {
            // `cursor-agent --help` carries no image/attachment flag, so the
            // honest thing is to say so in the transcript rather than answer
            // the text alone as though nothing had been attached.
            turn::notify_attachments_dropped(
                &db,
                &broadcaster,
                &session_id,
                "cursor-agent",
                message.attachments.len(),
            )
            .await;
        }

        // Per-session MCP wiring: a static env-reference entry in the
        // workspace `.cursor/mcp.json` (plus any user-defined servers riding
        // in the per-session file), the real token via env var at spawn.
        let mcp_wiring = config.mcp_config_path.as_deref().and_then(|path| {
            let wiring = mcp::parse_worker_mcp_config(path)?;
            match mcp::ensure_workspace_mcp_config(
                &config.working_dir,
                &wiring.url,
                &wiring.extra_servers,
            ) {
                Ok(_) => Some(wiring),
                Err(e) => {
                    tracing::warn!(session_id = %session_id, "cursor: MCP wiring skipped: {e}");
                    None
                }
            }
        });
        // cursor-agent has no system-prompt flag, so the composed prompt —
        // shared working-style rules, then any per-spawn suffix, then any
        // per-session override — is folded into the first turn's text.
        let system_prompt = turn::compose_system_prompt(&config);
        let args = build_cli_args(
            &model,
            &message.text,
            conversation_id.as_deref(),
            auto_approve,
            &system_prompt,
        );

        // The whole spawn env reaches the child (per-session variables, a
        // custom PATH, proxy settings); the MCP bearer token rides on top.
        // Its value must match the turn's — MCP approval hashes the
        // env-interpolated config.
        let mut env = config.env.clone();
        if let Some(wiring) = &mcp_wiring {
            env.insert(mcp::TOKEN_ENV_VAR.to_string(), wiring.token.clone());
        }

        let cancel = Arc::new(Notify::new());
        let cancel_for_task = cancel.clone();
        let retire = Arc::new(Notify::new());
        let retire_for_task = retire.clone();
        let runs = self.runs.clone();
        let sid = session_id.clone();
        let model_label = config.model.clone();
        let working_dir = config.working_dir.clone();

        let handle = tokio::spawn(async move {
            // Approval is sticky per server config but cheap to re-assert.
            if let Some(wiring) = &mcp_wiring {
                mcp::approve_workspace_servers(wiring, &cli_path, &working_dir).await;
            }
            let mut stream = CursorStream::default();
            let result = turn::run_turn(
                TurnSpec {
                    provider: "cursor",
                    cli_path: &cli_path,
                    args: &args,
                    env: &env,
                    working_dir: &working_dir,
                    model_label: &model_label,
                    session_id: &sid,
                    db: &db,
                    broadcaster: broadcaster.as_ref(),
                    timeout_secs: DEFAULT_TIMEOUT_SECS,
                    cancel: cancel_for_task,
                    retire: retire_for_task,
                    retire_grace_secs: turn::RETIRE_GRACE_SECS,
                    // An unauthenticated cursor-agent exits non-zero rather
                    // than printing an interactive prompt to stderr, so
                    // there is nothing to watch for.
                    stderr_markers: &[],
                    spawn_hint: None,
                    empty_exit_reason: "cursor-agent exited without output",
                    // The stream's `system` init frame carries the model, so
                    // the parser emits `Started` itself.
                    started_up_front: false,
                    // cursor-agent exits non-zero on some turns that in fact
                    // streamed a complete answer.
                    success_on_output: true,
                    plugins: Some(plugins.as_ref()),
                },
                &mut stream,
            )
            .await;

            runs.lock().await.remove(&sid);

            let _ = completion_tx
                .send(ProcessCompletion {
                    session_id: sid,
                    completed: result.completed,
                    error: result.error,
                    run_id,
                    error_kind: result.error_kind,
                    turn_end_only: false,
                })
                .await;
        });

        self.runs.lock().await.insert(
            session_id,
            CursorRun {
                handle,
                cancel,
                retire,
            },
        );
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

    /// Graceful stop for the terminal MCP tools (`complete_step`,
    /// `finish_card`, `wont_do_card`): the card this run was working has
    /// already been transitioned, so let the in-flight tool response land and
    /// give the agent a short window to close out before the child is wound
    /// down. `cursor-agent` has no stdin control channel (the harness spawns
    /// it with `Stdio::null()`), so the grace lives in `run_turn` rather than
    /// in an EOF the child could observe — see `TurnSpec::retire`.
    ///
    /// Without this the CLI keeps running its own tool loop long after the
    /// card was freed, and the orchestrator's next tick used to start a
    /// second worker on top of it.
    async fn shutdown_after_turn(&self, session_id: &str) {
        let retire = {
            let runs = self.runs.lock().await;
            runs.get(session_id).map(|r| r.retire.clone())
        };
        if let Some(r) = retire {
            tracing::info!(session_id = %session_id, "Retiring cursor run after turn");
            r.notify_one();
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

/// Adapts the `cursor-agent` stream-json parser to the shared turn harness.
#[derive(Default)]
struct CursorStream(parser::TurnState);

impl TurnStream for CursorStream {
    fn on_line(&mut self, json: &serde_json::Value) -> Vec<ProviderEvent> {
        parser::parse_stream_json(json, &mut self.0)
    }

    fn take_conversation_id(&mut self) -> Option<String> {
        self.0.conversation_id.take()
    }

    /// The CLI's `system` init frame gives the parser everything it needs to
    /// emit `Started` on its own.
    fn emitted_start(&self) -> bool {
        self.0.emitted_start
    }
}

/// Build the `cursor-agent` argument vector for one turn.
///
/// The prompt is passed as a positional argument after `--` so a prompt
/// that begins with `-` isn't mistaken for a flag. `auto` (or an empty
/// model) means "let Cursor choose", so `--model` is omitted.
/// `--stream-partial-output` streams text as live deltas; the parser
/// swallows the cumulative segment snapshots the CLI emits alongside them.
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
        "--stream-partial-output".to_string(),
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
        // Untrusted workspaces (e.g. under /tmp) silently drop MCP servers;
        // trusting matches the auto-approve intent for headless runs.
        args.push("--trust".to_string());
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

/// Cursor encodes thinking in the model id itself (e.g.
/// `claude-opus-4-8-thinking-high`), so the catalog builder is where that
/// naming convention becomes an explicit `reasoning` capability tag —
/// `ModelInfo::is_thinking` reads tags only and no longer sniffs ids.
fn model_capabilities(id: &str) -> Vec<String> {
    let mut capabilities = vec!["code".to_string()];
    if id.to_ascii_lowercase().contains("thinking") {
        capabilities.push("reasoning".to_string());
    }
    capabilities
}

fn model_info(name: String) -> ModelInfo {
    ModelInfo {
        display_name: format!("{name} (Cursor)"),
        capabilities: model_capabilities(&name),
        id: name,
        tier: 0,
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
        ("grok-4.5-high", "Grok 4.5 High (Cursor)"),
    ]
    .into_iter()
    .map(|(id, name)| ModelInfo {
        id: id.into(),
        display_name: name.into(),
        capabilities: model_capabilities(id),
        tier: 0,
    })
    .collect()
}

/// Append `extras` to `base`, skipping ids already present (preserving order).
fn merge_additional_models(base: Vec<ModelInfo>, extras: Vec<String>) -> Vec<ModelInfo> {
    turn::merge_additional_models(base, extras, model_info)
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
        assert!(args.contains(&"--stream-partial-output".to_string()));
        assert!(args.contains(&"--force".to_string()));
        assert!(args.contains(&"--trust".to_string()));
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
        assert!(!args.iter().any(|a| a == "--trust"));
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
    fn thinking_ids_get_reasoning_capability_tag() {
        let m = model_info("claude-opus-4-8-thinking-high".into());
        assert!(m.is_thinking(), "thinking id must be tagged reasoning");
        let plain = model_info("gpt-5.3-codex".into());
        assert!(!plain.is_thinking());
        // The seed catalog carries the tags too.
        let seeds = default_models();
        let tagged = seeds
            .iter()
            .find(|m| m.id == "claude-4.5-sonnet-thinking")
            .expect("seed present");
        assert!(tagged.is_thinking());
    }

    #[test]
    fn default_models_are_prefix_free_ids() {
        // ProviderInfo ids must be bare (the registry adds the `cursor:`
        // prefix when building full model ids).
        for m in default_models() {
            assert!(!m.id.contains(':'), "id {} should be prefix-free", m.id);
        }
    }

    #[test]
    fn default_models_drops_stale_grok_and_seeds_current() {
        let models = default_models();
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        // grok-4.3 was retired upstream, replaced by the grok-4.5 family.
        assert!(
            !ids.contains(&"grok-4.3"),
            "stale grok-4.3 must not be seeded"
        );
        assert!(ids.contains(&"grok-4.5-high"));
    }
}
