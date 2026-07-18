//! Moonshot AI Kimi Code CLI (`kimi`) agent provider.
//!
//! Drives sessions through Kimi Code's non-interactive prompt mode. Like the
//! Cursor and Grok providers — and unlike Claude's long-lived duplex child —
//! `kimi` is invoked **once per turn**:
//!
//! ```text
//! kimi --prompt <prompt> --output-format stream-json [--model M] [--session SESS]
//! ```
//!
//! Each invocation streams newline-delimited JSON (OpenAI-style `assistant` /
//! `tool` role messages plus `meta` frames — see [`parser`] docs) which
//! [`parser`] turns into the unified [`ProviderEvent`] stream. Kimi's session
//! id is captured from the trailing `session.resume_hint` frame and emitted
//! on `Completed` so the next turn can resume the same conversation with
//! `--session`. Prompt mode always auto-approves tool actions (the CLI
//! rejects `--prompt` combined with `--yolo`), so terminal tools are denied
//! at parse time exactly like the grok provider.
//!
//! The CLI has no system-prompt or MCP flags, so — as with Cursor — the
//! shared WORKING_STYLE rules (plus any per-session override) are folded into
//! the first turn's prompt, and user-defined MCP servers are not wired in.
//!
//! Auth is host-level: `kimi login` (device-code flow) or a
//! `~/.kimi-code/config.toml` with a `type = "kimi"` provider. The optional
//! `api_key` / `base_url` plugin settings are injected as `KIMI_API_KEY` /
//! `KIMI_BASE_URL` for config files that use the documented env fallback.
//! An unauthenticated host fails fast with "No model configured" on stderr,
//! which is mapped to a friendly crash message.
//!
//! Multi-account works exactly like the Grok provider: a session's model id
//! may carry an `@<account_id>` suffix, which resolves to a per-account
//! `KIMI_CODE_HOME` (and, for `api_key` accounts, a `KIMI_API_KEY`) injected
//! at spawn time. A bare model id uses the host's ambient `~/.kimi-code`
//! credentials.
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

use crate::plugin::settings::PluginSettingsStore;
use crate::provider::agent::{AgentProvider, ProcessCompletion, SendMessageContext, emit_event};
use crate::provider::registry::split_model_account;
use crate::provider::stream::{ModelInfo, ProviderEvent};

/// Default per-turn timeout. Kimi turns can take several tool steps, so this
/// is generous; an unauthenticated host fails in seconds instead.
const DEFAULT_TIMEOUT_SECS: u64 = 600;
/// Default CLI binary name; overridable via the `cli_path` setting.
const DEFAULT_CLI: &str = "kimi";
/// Cap on stderr bytes captured for a crash message.
const MAX_STDERR_BYTES: usize = 16 * 1024;
/// How long a model-discovery probe (success or failure) is cached, so the
/// picker doesn't shell out on every render.
const MODEL_DISCOVERY_TTL: Duration = Duration::from_secs(60);
/// Bound on how long the discovery subprocess may run.
const MODEL_DISCOVERY_TIMEOUT_SECS: u64 = 10;
/// Substring of the error kimi prints when no model/credential is configured
/// ("No model configured. Run `kimi` and use /login to sign in, ...").
const AUTH_NEEDED_MARKER: &str = "No model configured";

/// Per-session tracking for an in-flight `kimi` turn.
struct KimiRun {
    handle: JoinHandle<()>,
    cancel: Arc<Notify>,
}

/// TTL cache for the model-discovery probe.
struct DiscoveryCache {
    fetched_at: Instant,
    models: Option<Vec<String>>,
}

/// `AgentProvider` backed by per-turn `kimi` invocations.
pub struct KimiProvider {
    settings: PluginSettingsStore,
    runs: Arc<Mutex<HashMap<String, KimiRun>>>,
    discovery_cache: Arc<Mutex<Option<DiscoveryCache>>>,
    /// DB handle for multi-account support: `dynamic_models` enumerates the
    /// stored accounts and `send_message` resolves the per-account credential
    /// to inject. `None` in tests / no-DB registrations keeps the
    /// single-(Default-)account behaviour.
    db: Option<crate::db::Db>,
}

impl KimiProvider {
    pub fn new(settings: PluginSettingsStore) -> Self {
        KimiProvider {
            settings,
            runs: Arc::new(Mutex::new(HashMap::new())),
            discovery_cache: Arc::new(Mutex::new(None)),
            db: None,
        }
    }

    /// Attach a DB handle so the provider can resolve Kimi accounts.
    pub fn with_db(mut self, db: crate::db::Db) -> Self {
        self.db = Some(db);
        self
    }

    /// Resolve `account_id` to its credential and add the env the spawned
    /// `kimi` CLI needs to run as that account: every account gets an
    /// isolated `KIMI_CODE_HOME` (its `config_dir`), and an `api_key`
    /// account additionally injects `KIMI_API_KEY`. An account id that no
    /// longer exists (deleted out from under a live session) is a hard error
    /// rather than a silent fall back to host credentials — a turn must
    /// never bill the wrong account.
    async fn inject_account_env(
        &self,
        account_id: &str,
        env: &mut HashMap<String, String>,
    ) -> anyhow::Result<()> {
        let Some(db) = &self.db else {
            return Ok(());
        };
        let account = db
            .get_kimi_account(account_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("kimi account not found: {account_id}"))?;
        if let Some(dir) = &account.config_dir {
            std::fs::create_dir_all(dir).ok();
            env.insert("KIMI_CODE_HOME".into(), dir.clone());
        }
        if account.kind == "api_key" {
            env.insert("KIMI_API_KEY".into(), account.credential.clone());
        }
        Ok(())
    }

    /// One labelled variant of each base model per stored account
    /// (`<model>@<account_id>`, shown as `[Account] Model`). Mirrors the
    /// Grok provider's `account_scoped_models`.
    async fn account_scoped_models(&self, base: &[ModelInfo]) -> Vec<ModelInfo> {
        let Some(db) = &self.db else {
            return Vec::new();
        };
        let accounts = match db.list_kimi_accounts().await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!("kimi: failed to list accounts for model catalog: {e}");
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        for acct in &accounts {
            for m in base {
                out.push(ModelInfo {
                    id: format!("{}@{}", m.id, acct.id),
                    display_name: format!("[{}] {}", acct.name, m.display_name),
                    capabilities: m.capabilities.clone(),
                    tier: m.tier,
                });
            }
        }
        out
    }
    /// Run the discovery command and return model aliases, going through the
    /// TTL cache. `Some(list)` on success (possibly empty), `None` when the
    /// last probe failed and the caller should fall back to the static seed.
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
impl AgentProvider for KimiProvider {
    fn id(&self) -> &str {
        "kimi"
    }

    async fn dynamic_models(&self) -> Option<Vec<ModelInfo>> {
        let settings = match self.settings.load().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("kimi: failed to load settings for model list: {e}");
                HashMap::new()
            }
        };
        let cli_path =
            setting_str(&settings, "cli_path").unwrap_or_else(|| DEFAULT_CLI.to_string());
        let extras = setting_str_list(&settings, "additional_models");
        let discover = setting_bool(&settings, "discover_models").unwrap_or(true);

        let base = if discover {
            match self.discovered_models(&cli_path).await {
                // The config-default entry stays first so an alias-free setup
                // still has a working selection.
                Some(ids) if !ids.is_empty() => default_models()
                    .into_iter()
                    .chain(ids.into_iter().map(model_info))
                    .collect(),
                // Discovery failed or returned nothing usable → static seed.
                _ => default_models(),
            }
        } else {
            default_models()
        };

        let base = merge_additional_models(base, extras);
        let account_variants = self.account_scoped_models(&base).await;
        Some(base.into_iter().chain(account_variants).collect())
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
            // kimi runs its own tool loop; the plugin host (MCP tool
            // execution) isn't wired in — the CLI has no MCP flags.
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
        let default_model =
            setting_str(&settings, "default_model").filter(|m| !crate::provider::is_auto_model(m));
        // Strip the `kimi:` prefix and peel off any `@<account_id>` suffix.
        // A model with no suffix is the implicit Default account: nothing
        // injected, host credentials apply. `kimi:<alias>` selects a
        // config.toml model alias; the `default` pseudo-model omits
        // `--model` so the CLI uses its own configured default.
        let (session_model, account_id) = resolve_model_and_account(&config.model);
        let model = session_model.or(default_model);

        let mut env = config.env.clone();
        if let Some(key) = setting_str(&settings, "api_key") {
            env.insert("KIMI_API_KEY".into(), key);
        }
        if let Some(base_url) = setting_str(&settings, "base_url") {
            env.insert("KIMI_BASE_URL".into(), base_url);
        }
        // Account env last: a per-account KIMI_CODE_HOME / KIMI_API_KEY
        // overrides the plugin-level key so the turn runs (and bills) as the
        // selected account.
        if let Some(account_id) = account_id {
            self.inject_account_env(&account_id, &mut env).await?;
        }

        if !message.attachments.is_empty() {
            tracing::warn!(
                session_id = %session_id,
                "kimi: dropping {} attachment(s) — the kimi provider is text-only for now",
                message.attachments.len()
            );
        }
        if config.mcp_config_path.is_some() {
            tracing::debug!(
                session_id = %session_id,
                "kimi: MCP servers not wired — the kimi CLI exposes no MCP flags"
            );
        }

        // No system-prompt flag: the shared working-style rules (plus any
        // per-session override) ride the first turn's prompt, Cursor-style.
        let mut system_prompt = crate::provider::WORKING_STYLE.to_string();
        if let Some(custom) = config
            .system_prompt_override
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            system_prompt.push('\n');
            system_prompt.push_str(custom);
        }

        let args = build_cli_args(
            model.as_deref(),
            &message.text,
            conversation_id.as_deref(),
            &system_prompt,
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
                    error: None,
                })
                .await;
        });

        self.runs
            .lock()
            .await
            .insert(session_id, KimiRun { handle, cancel });
        Ok(())
    }

    async fn cancel(&self, session_id: &str) {
        let cancel = {
            let runs = self.runs.lock().await;
            runs.get(session_id).map(|r| r.cancel.clone())
        };
        if let Some(c) = cancel {
            tracing::info!(session_id = %session_id, "Cancelling kimi run");
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
                    "wait_for_termination timed out for kimi run"
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

/// Arguments for a single `kimi` turn.
struct TurnArgs<'a> {
    cli_path: &'a str,
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

/// Spawn `kimi` for one turn, stream its stdout into provider events, and
/// emit a terminal `Completed` / `Crashed`. Returns `true` on a clean
/// completion, `false` otherwise.
async fn run_turn(args: TurnArgs<'_>) -> bool {
    let TurnArgs {
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
    } = args;

    tracing::info!(
        session_id = %session_id,
        "Spawning kimi: {} {}",
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
            emit_started(db, broadcaster, session_id, model_label).await;
            crash(
                db,
                broadcaster,
                session_id,
                &format!(
                    "failed to spawn '{cli_path}': {e}. Install Kimi Code with \
                     `curl -fsSL https://code.kimi.com/kimi-code/install.sh | bash` \
                     or point the plugin's CLI Path setting at the binary."
                ),
                None,
            )
            .await;
            return false;
        }
    };

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take();

    // Drain stderr concurrently into a bounded buffer for crash reporting.
    // Unlike grok, an unauthenticated kimi doesn't hang on a device prompt —
    // it exits immediately with "No model configured" — so no auth watcher
    // is needed here.
    let stderr_task = stderr.map(|s| {
        tokio::spawn(async move {
            let mut buf = String::new();
            let mut lines = BufReader::new(s).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if buf.len() < MAX_STDERR_BYTES {
                    if !buf.is_empty() {
                        buf.push('\n');
                    }
                    buf.push_str(&line);
                }
            }
            buf.trim().to_string()
        })
    });

    // Emit Started up front so the UI shows an agent-start; kimi's stream has
    // no init frame to derive one from (system.version carries no model).
    emit_started(db, broadcaster, session_id, model_label).await;

    let mut conversation_id: Option<String> = None;
    // Terminal-tool calls denied at parse time; their ids are tracked here so
    // the CLI's real result line can be dropped (see kimi parser).
    let mut denied_tool_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

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
                            tracing::debug!(session_id = %session_id, "kimi: non-JSON stdout line ignored");
                            continue;
                        };
                        for event in parser::parse_stream_json(
                            &json,
                            &mut conversation_id,
                            &mut denied_tool_ids,
                        ) {
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
    let stderr_text = match stderr_task {
        Some(t) => t.await.unwrap_or_default(),
        None => String::new(),
    };

    match outcome {
        TurnOutcome::Eof => {
            let ok = status.map(|s| s.success()).unwrap_or(false);
            if ok {
                emit_event(
                    db,
                    broadcaster,
                    session_id,
                    ProviderEvent::Completed { conversation_id },
                )
                .await;
                true
            } else {
                let reason = if stderr_text.contains(AUTH_NEEDED_MARKER) {
                    "Kimi Code isn't signed in on this host. Run `kimi login` (or add a \
                     provider to ~/.kimi-code/config.toml / set an API key in the plugin \
                     settings), then try again."
                        .to_string()
                } else if stderr_text.is_empty() {
                    "kimi exited without a successful result".to_string()
                } else {
                    stderr_text
                };
                crash(db, broadcaster, session_id, &reason, exit_code(status)).await;
                false
            }
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
                &format!("kimi turn exceeded {timeout_secs}s timeout"),
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
    model: &str,
) {
    emit_event(
        db,
        broadcaster,
        session_id,
        ProviderEvent::Started {
            model: model.to_string(),
            conversation_id: None,
            metadata: serde_json::json!({}),
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

/// Probe the CLI for its configured model aliases via
/// `kimi provider list --json`.
async fn probe_cli_models(cli_path: &str) -> Option<Vec<String>> {
    let mut cmd = Command::new(cli_path);
    cmd.args(["provider", "list", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("kimi: model discovery spawn failed: {e}");
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
            tracing::warn!("kimi: model discovery failed: {e}");
            return None;
        }
        Err(_) => {
            tracing::warn!("kimi: model discovery timed out");
            return None;
        }
    };

    if !output.status.success() {
        tracing::warn!(
            "kimi: model discovery exited with {:?}",
            output.status.code()
        );
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parser::parse_cli_models(&text)
}

/// Build the argv for one turn. `model` is `None` for the config-default
/// selection (no `--model` flag). On the first turn (no conversation id) the
/// system prompt is folded in ahead of the user's text — kimi has no
/// system-prompt flag; a resumed session already carries the rules.
fn build_cli_args(
    model: Option<&str>,
    prompt: &str,
    conversation_id: Option<&str>,
    system_prompt: &str,
) -> Vec<String> {
    let effective_prompt = if conversation_id.is_none() && !system_prompt.is_empty() {
        format!("{system_prompt}\n\n{prompt}")
    } else {
        prompt.to_string()
    };
    // Prompt mode auto-approves tool actions on its own; the CLI rejects an
    // explicit `--yolo` alongside `--prompt`.
    let mut args = vec![
        "--prompt".to_string(),
        effective_prompt,
        "--output-format".to_string(),
        "stream-json".to_string(),
    ];
    if let Some(model) = model {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if let Some(cid) = conversation_id {
        args.push("--session".to_string());
        args.push(cid.to_string());
    }
    args
}

/// Split a session's model id into the `--model` value and any
/// `@<account_id>` suffix. Accepts `kimi:`-prefixed and bare ids; the
/// `default`/`auto` pseudo-models (and empty) yield `None` so the caller
/// falls back to the configured default (and ultimately to omitting
/// `--model`).
fn resolve_model_and_account(raw: &str) -> (Option<String>, Option<String>) {
    let stripped = raw.strip_prefix("kimi:").unwrap_or(raw);
    let (base, account) = split_model_account(stripped);
    let model = Some(base.to_string()).filter(|m| !crate::provider::is_auto_model(m));
    (model, account.map(str::to_string))
}

fn model_info(name: String) -> ModelInfo {
    ModelInfo {
        display_name: format!("{name} (Kimi)"),
        id: name,
        capabilities: vec!["code".into()],
        tier: 0,
    }
}

/// The static seed: only the config-default pseudo-model. Kimi's `--model`
/// takes user-defined aliases from `~/.kimi-code/config.toml`, so there are
/// no universally-valid ids to seed; discovery (`kimi provider list --json`)
/// and the `additional_models` setting supply the real aliases.
pub fn default_models() -> Vec<ModelInfo> {
    vec![ModelInfo {
        id: "default".into(),
        display_name: "Default (Kimi config)".into(),
        capabilities: vec!["code".into()],
        tier: 0,
    }]
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
    fn resolve_model_and_account_strips_prefix_and_splits_account() {
        assert_eq!(
            resolve_model_and_account("kimi:kimi-for-coding"),
            (Some("kimi-for-coding".into()), None)
        );
        assert_eq!(resolve_model_and_account("kimi:"), (None, None));
        assert_eq!(resolve_model_and_account("kimi:default"), (None, None));
        assert_eq!(resolve_model_and_account("kimi:auto"), (None, None));
        assert_eq!(
            resolve_model_and_account("kimi:default@kacc_1"),
            (None, Some("kacc_1".into()))
        );
        assert_eq!(
            resolve_model_and_account("kimi:kimi-k2-thinking@kacc_1"),
            (Some("kimi-k2-thinking".into()), Some("kacc_1".into()))
        );
        assert_eq!(
            resolve_model_and_account("kimi-for-coding"),
            (Some("kimi-for-coding".into()), None)
        );
    }

    #[test]
    fn build_args_sets_prompt_mode_and_stream_json() {
        let args = build_cli_args(None, "hello", None, "");
        assert_eq!(args[0], "--prompt");
        assert_eq!(args[1], "hello");
        let f = args.iter().position(|a| a == "--output-format").unwrap();
        assert_eq!(args[f + 1], "stream-json");
        assert!(!args.iter().any(|a| a == "--model"));
        assert!(!args.iter().any(|a| a == "--session"));
        // Prompt mode auto-approves; --yolo alongside --prompt is a CLI error.
        assert!(!args.iter().any(|a| a == "--yolo"));
    }

    #[test]
    fn build_args_includes_model_and_session() {
        let args = build_cli_args(Some("kimi-for-coding"), "do it", Some("sess-7"), "");
        let m = args.iter().position(|a| a == "--model").unwrap();
        assert_eq!(args[m + 1], "kimi-for-coding");
        let s = args.iter().position(|a| a == "--session").unwrap();
        assert_eq!(args[s + 1], "sess-7");
    }

    #[test]
    fn first_turn_prepends_system_prompt_but_resume_does_not() {
        let first = build_cli_args(None, "do it", None, crate::provider::WORKING_STYLE);
        assert!(first[1].contains("# Working style"));
        assert!(first[1].ends_with("do it"));

        let resume = build_cli_args(
            None,
            "do it",
            Some("sess-7"),
            crate::provider::WORKING_STYLE,
        );
        assert_eq!(resume[1], "do it");
    }

    #[test]
    fn merge_additional_models_dedups_against_seed() {
        let merged = merge_additional_models(
            default_models(),
            vec!["default".into(), "my-alias".into(), "my-alias".into()],
        );
        let ids: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["default", "my-alias"]);
    }

    #[test]
    fn default_models_are_prefix_free() {
        for m in default_models() {
            assert!(!m.id.contains(':'), "id {} should be prefix-free", m.id);
        }
    }
}
