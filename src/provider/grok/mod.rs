//! Grok CLI (`grok`) agent provider.
//!
//! Drives sessions through Grok's headless mode. Like the Cursor provider —
//! and unlike Claude's long-lived duplex child — `grok` is invoked **once
//! per turn** in single-prompt streaming mode:
//!
//! ```text
//! grok --single=<prompt> --output-format=streaming-json \
//!     [--model=M] [--resume=SESS] [--effort=LEVEL] --always-approve
//! ```
//!
//! Each invocation streams newline-delimited JSON (`text` / `thought` /
//! `tool_call` / `tool` / `usage` / `end`) which [`parser`] turns into the
//! unified [`ProviderEvent`] stream. Grok's `sessionId` is captured from the
//! `end` frame and emitted on `Completed` so the next turn can resume the
//! same conversation with `--resume` (grok 1.0's `--session-id` only names a
//! brand-new session and errors on an existing id).
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
use crate::provider::registry::split_model_account;
use crate::provider::stream::{ModelInfo, ProviderEvent};
use crate::provider::turn::{
    self, StderrMarker, TurnSpec, TurnStream, setting_bool, setting_str, setting_str_list,
};

/// Default CLI binary name; overridable via the `cli_path` setting.
const DEFAULT_CLI: &str = "grok";
/// Where to look for `grok` when it isn't on the server's PATH. The PeckBoard
/// service PATH often predates the CLI install, since installers only extend
/// an interactive shell's rc file.
const CLI_FALLBACK_DIRS: &[&str] = &[
    "~/.local/bin",
    "~/.npm-global/bin",
    "~/.bun/bin",
    "/usr/local/bin",
];
/// How long a model-discovery probe (success or failure) is cached, so the
/// picker doesn't shell out on every render.
const MODEL_DISCOVERY_TTL: Duration = Duration::from_secs(60);
/// Bound on how long `grok models` may run.
const MODEL_DISCOVERY_TIMEOUT_SECS: u64 = 10;
/// Grok prints its device-login URL to stderr when the account it runs as
/// isn't signed in. Seeing it means the turn would otherwise block forever on
/// "Waiting for authorization...", so the harness fast-fails on it. grok 1.0
/// headless instead fast-fails with "Error: Not signed in." on stderr (and no
/// stdout error frame), so that marker classifies the crash as an auth
/// failure too.
const STDERR_MARKERS: &[StderrMarker] = &[
    StderrMarker {
        marker: "accounts.x.ai/oauth2/device",
        message: "This Grok account isn't signed in. Open Settings \u{2192} Grok accounts and \
                  complete the browser sign-in, then try again.",
        kind: crate::provider::stream::CrashKind::AuthExpired,
        abort: true,
    },
    StderrMarker {
        marker: "Not signed in",
        message: "This Grok account isn't signed in. Open Settings \u{2192} Grok accounts and \
                  complete the sign-in, then try again.",
        kind: crate::provider::stream::CrashKind::AuthExpired,
        abort: true,
    },
];

/// Per-session tracking for an in-flight `grok` turn.
struct GrokRun {
    handle: JoinHandle<()>,
    cancel: Arc<Notify>,
    /// Graceful "your card is done, wrap up" signal — see `TurnSpec::retire`.
    retire: Arc<Notify>,
}

/// TTL cache for one `grok models` probe (host or a single account).
struct DiscoveryCache {
    fetched_at: Instant,
    models: Option<Vec<String>>,
}

/// `AgentProvider` backed by per-turn `grok` invocations.
pub struct GrokProvider {
    runs: Arc<Mutex<HashMap<String, GrokRun>>>,
    /// DB handle for multi-account support: `dynamic_models` enumerates the
    /// stored accounts and `send_message` resolves the per-account credential
    /// to inject. `None` in tests / no-DB registrations keeps the
    /// single-(Default-)account behaviour.
    db: Option<crate::db::Db>,
    /// Plugin settings, for the `cli_path` / `discover_models` overrides.
    /// `None` in tests / no-plugin registrations, which then use the default
    /// binary name and discovery on.
    settings: Option<PluginSettingsStore>,
    /// Per-scope (`""` = host, else account id) cache of `grok models` probes.
    discovery_cache: Arc<Mutex<HashMap<String, DiscoveryCache>>>,
}

impl GrokProvider {
    pub fn new() -> Self {
        GrokProvider {
            runs: Arc::new(Mutex::new(HashMap::new())),
            db: None,
            settings: None,
            discovery_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Attach a DB handle so the provider can resolve Grok accounts.
    pub fn with_db(mut self, db: crate::db::Db) -> Self {
        self.db = Some(db);
        self
    }

    /// Attach the plugin settings store so the `cli_path` setting is honoured.
    pub fn with_settings(mut self, settings: PluginSettingsStore) -> Self {
        self.settings = Some(settings);
        self
    }

    /// Load plugin settings, or an empty map when unset / unloadable.
    async fn load_settings(&self) -> HashMap<String, serde_json::Value> {
        match &self.settings {
            Some(store) => match store.load().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("grok: failed to load settings: {e}");
                    HashMap::new()
                }
            },
            None => HashMap::new(),
        }
    }

    /// The `grok` executable to spawn: the plugin's `cli_path` setting when
    /// one is configured, else the default name — either way resolved against
    /// the server's PATH and the usual install locations.
    async fn cli_path(&self) -> String {
        let settings = self.load_settings().await;
        let configured = setting_str(&settings, "cli_path");
        turn::resolve_cli_path(
            &configured.unwrap_or_else(|| DEFAULT_CLI.to_string()),
            CLI_FALLBACK_DIRS,
        )
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

    /// Run `grok models` under `env` and return model ids, via the per-scope
    /// TTL cache (`scope`: "" = host ambient credentials, or a grok account
    /// id whose env selects its `GROK_HOME` / `XAI_API_KEY`). `Some(list)` on
    /// success (possibly empty), `None` when the last probe failed and the
    /// caller should fall back.
    async fn discovered_models(
        &self,
        cli_path: &str,
        scope: &str,
        env: &HashMap<String, String>,
    ) -> Option<Vec<String>> {
        {
            let cache = self.discovery_cache.lock().await;
            if let Some(entry) = cache.get(scope)
                && entry.fetched_at.elapsed() < MODEL_DISCOVERY_TTL
            {
                return entry.models.clone();
            }
        }
        let result = probe_cli_models(cli_path, env).await;
        let mut cache = self.discovery_cache.lock().await;
        cache.insert(
            scope.to_string(),
            DiscoveryCache {
                fetched_at: Instant::now(),
                models: result.clone(),
            },
        );
        result
    }

    /// One labelled variant per stored account (`<model>@<account_id>`, shown
    /// as `[Account] Model`). Each account is probed under its own
    /// `GROK_HOME` / `XAI_API_KEY` because the CLI catalog is auth-scoped
    /// (OAuth lists `grok-4.6`; API-key / unauth often only `grok-4.5`).
    /// Discovery off/failed falls back to mirroring `base`.
    async fn account_scoped_models(
        &self,
        base: &[ModelInfo],
        cli_path: &str,
        discover: bool,
    ) -> Vec<ModelInfo> {
        let Some(db) = &self.db else {
            return Vec::new();
        };
        let accounts = match db.list_grok_accounts().await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!("grok: failed to list accounts for model catalog: {e}");
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        for acct in &accounts {
            let discovered = if discover {
                let mut env = HashMap::new();
                match self.inject_account_env(&acct.id, &mut env).await {
                    Ok(()) => self.discovered_models(cli_path, &acct.id, &env).await,
                    Err(e) => {
                        tracing::warn!("grok: skip discovery for account {}: {e}", acct.id);
                        None
                    }
                }
            } else {
                None
            };
            let acct_base: Vec<ModelInfo> = match discovered {
                Some(ids) if !ids.is_empty() => ids
                    .into_iter()
                    .enumerate()
                    .map(|(i, id)| model_info(id, i as i32))
                    .collect(),
                _ => base.to_vec(),
            };
            for m in acct_base {
                out.push(ModelInfo {
                    id: format!("{}@{}", m.id, acct.id),
                    display_name: format!("[{}] {}", acct.name, m.display_name),
                    capabilities: m.capabilities,
                    tier: m.tier,
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
        let settings = self.load_settings().await;
        let cli_path = turn::resolve_cli_path(
            &setting_str(&settings, "cli_path").unwrap_or_else(|| DEFAULT_CLI.to_string()),
            CLI_FALLBACK_DIRS,
        );
        let extras = setting_str_list(&settings, "additional_models");
        let discover = setting_bool(&settings, "discover_models").unwrap_or(true);

        let base = if discover {
            match self.discovered_models(&cli_path, "", &HashMap::new()).await {
                Some(ids) if !ids.is_empty() => ids
                    .into_iter()
                    .enumerate()
                    .map(|(i, id)| model_info(id, i as i32))
                    .collect(),
                _ => default_models(),
            }
        } else {
            default_models()
        };

        let base = merge_additional_models(base, extras);
        let account_variants = self.account_scoped_models(&base, &cli_path, discover).await;
        Some(base.into_iter().chain(account_variants).collect())
    }

    async fn auth_configured(&self) -> Option<bool> {
        if let Some(db) = &self.db {
            match db.list_grok_accounts().await {
                Ok(accounts) if !accounts.is_empty() => return Some(true),
                Ok(_) => {}
                // Can't tell — don't warn on a transient DB error.
                Err(_) => return None,
            }
        }
        if std::env::var("XAI_API_KEY").is_ok_and(|v| !v.is_empty()) {
            return Some(true);
        }
        // Host-level login: the CLI writes auth.json into ~/.grok.
        let host = dirs::home_dir()
            .map(|h| h.join(".grok").join("auth.json"))
            .is_some_and(|p| std::fs::metadata(&p).is_ok_and(|m| m.len() > 0));
        Some(host)
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
            // grok runs its own tool loop, so the WASM plugin tool host
            // stays unwired as a *tool* provider; peckboard MCP tools reach
            // it via the workspace `.mcp.json` + a per-spawn token env var
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

        let cli_path = self.cli_path().await;

        // Strip the `grok:` prefix, then peel off any `@<account_id>` suffix
        // and resolve it to the credential env. A model with no suffix is the
        // implicit Default account: nothing injected, host credentials apply.
        let stripped = config
            .model
            .strip_prefix("grok:")
            .map(|m| m.to_string())
            .unwrap_or_else(|| config.model.clone());
        let (base_model, account_id) = split_model_account(&stripped);
        let model = effective_model(base_model);

        let mut env = config.env.clone();
        if let Some(account_id) = account_id {
            self.inject_account_env(account_id, &mut env).await?;
        }

        if !message.attachments.is_empty() {
            // `grok --help` carries no image/attachment flag, so say so in
            // the transcript rather than silently answering the text alone.
            turn::notify_attachments_dropped(
                &db,
                &broadcaster,
                &session_id,
                "grok",
                message.attachments.len(),
            )
            .await;
        }
        // MCP wiring: mirror the peckboard server AND any user-defined
        // servers (Settings → MCP Servers, already provider-filtered into the
        // per-session worker-mcp file at dispatch) into the workspace
        // `.mcp.json`, which Grok Build loads as a compatibility source. The
        // bearer token stays out of the file — it rides an env var the file
        // references. Best-effort: the turn runs without MCP on any failure.
        if !config.working_dir.is_empty()
            && let Some(path) = config.mcp_config_path.as_deref()
        {
            let wiring = mcp::parse_worker_mcp_config(path);
            // A config without the peckboard entry still contributes its
            // user-defined servers.
            let extras = match &wiring {
                Some(w) => w.extra_servers.clone(),
                None => mcp::extra_servers_from_worker_config(path),
            };
            match mcp::ensure_workspace_mcp_json(
                &config.working_dir,
                wiring.as_ref().map(|w| w.url.as_str()),
                &extras,
            ) {
                Ok(_) => {
                    if let Some(w) = &wiring {
                        env.insert(mcp::TOKEN_ENV_VAR.to_string(), w.token.clone());
                    }
                }
                Err(e) => {
                    tracing::warn!(session_id = %session_id, "grok: MCP wiring skipped: {e}")
                }
            }
        }

        let args = build_cli_args(
            &model,
            &message.text,
            conversation_id.as_deref(),
            config.effort.as_deref(),
            &turn::compose_system_prompt(&config),
        );

        let cancel = Arc::new(Notify::new());
        let cancel_for_task = cancel.clone();
        let retire = Arc::new(Notify::new());
        let retire_for_task = retire.clone();
        let runs = self.runs.clone();
        let sid = session_id.clone();
        let model_label = config.model.clone();
        let working_dir = config.working_dir.clone();

        let handle = tokio::spawn(async move {
            let mut stream = GrokStream::default();
            let result = turn::run_turn(
                TurnSpec {
                    provider: "grok",
                    cli_path: &cli_path,
                    args: &args,
                    env: &env,
                    working_dir: &working_dir,
                    model_label: &model_label,
                    session_id: &sid,
                    db: &db,
                    broadcaster: broadcaster.as_ref(),
                    // No wall-clock bound: a grok turn is the agent working,
                    // and a long tool step is not a failure to reap.
                    timeout_secs: None,
                    cancel: cancel_for_task,
                    retire: retire_for_task,
                    retire_grace_secs: turn::RETIRE_GRACE_SECS,
                    stderr_markers: STDERR_MARKERS,
                    spawn_hint: Some(
                        "Install the Grok CLI, or point the plugin's CLI Path setting \
                         at the binary.",
                    ),
                    empty_exit_reason: "grok exited without a successful result",
                    // grok's streaming-json has no init frame to derive a
                    // Started from, so the harness emits one up front.
                    started_up_front: true,
                    success_on_output: false,
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
            GrokRun {
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
            tracing::info!(session_id = %session_id, "Cancelling grok run");
            c.notify_one();
        }
    }

    /// Graceful stop for the terminal MCP tools: the card this run was
    /// working has already been transitioned, so let the in-flight tool
    /// response land and give the agent a short window to close out before
    /// the child is wound down. See `TurnSpec::retire`.
    async fn shutdown_after_turn(&self, session_id: &str) {
        let retire = {
            let runs = self.runs.lock().await;
            runs.get(session_id).map(|r| r.retire.clone())
        };
        if let Some(r) = retire {
            tracing::info!(session_id = %session_id, "Retiring grok run after turn");
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

/// Adapts the grok streaming-json parser to the shared turn harness.
#[derive(Default)]
struct GrokStream {
    conversation_id: Option<String>,
    error: Option<String>,
}

impl TurnStream for GrokStream {
    fn on_line(&mut self, json: &serde_json::Value) -> Vec<ProviderEvent> {
        if let Some(reason) = parser::error_reason(json) {
            // grok still emits an `end` frame after an error, so remember the
            // last one and let the stream finish naturally.
            self.error = Some(reason);
            return Vec::new();
        }
        parser::parse_stream_json(json, &mut self.conversation_id)
    }

    fn take_conversation_id(&mut self) -> Option<String> {
        self.conversation_id.take()
    }

    fn take_error(&mut self) -> Option<String> {
        self.error.take()
    }
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
    system_prompt: &str,
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
        // Resuming MUST use `--resume`: grok 1.0's `--session-id` only names
        // a brand-new session and hard-errors with "Session ID … is already
        // in use" when the id exists (verified against grok 1.0.3).
        args.push(format!("--resume={cid}"));
    }
    if let Some(effort) = effort.map(str::trim).filter(|e| !e.is_empty()) {
        args.push(format!("--effort={effort}"));
    }
    // Grok's flag takes the whole system prompt, so the caller composes it
    // (shared working-style rules, then the per-spawn suffix, then any
    // per-session override) — the same layering Claude applies.
    args.push(format!("--system-prompt-override={system_prompt}"));
    args
}

/// Safe fallback model for empty / legacy ids. Kept as `grok-4.5` because
/// that id is in every auth-scoped CLI catalog (OAuth, API key, unauth);
/// `grok-4.6` is OAuth-only and would hard-error on API-key turns.
const DEFAULT_MODEL: &str = "grok-4.5";

/// The default model grok 0.x shipped with; grok 1.0 removed it from the
/// catalog, and passing it via `--model` now hard-errors with "unknown model
/// id". Stored sessions and cards may still carry it, so it maps to the
/// current default instead of failing every turn.
const LEGACY_DEFAULT_MODEL: &str = "grok-build";

/// Map a stored model id to one the current CLI accepts.
fn effective_model(base_model: &str) -> String {
    if base_model.is_empty() || base_model == LEGACY_DEFAULT_MODEL {
        DEFAULT_MODEL.to_string()
    } else {
        base_model.to_string()
    }
}

/// The built-in seed model list used when `discover_models` is off or
/// `grok models` fails. Live catalogs come from the CLI (see
/// [`GrokProvider::dynamic_models`]); this seed only fills the gap.
pub fn default_models() -> Vec<ModelInfo> {
    vec![
        model_info("grok-4.6".into(), 0),
        model_info("grok-4.5".into(), 1),
    ]
}

/// Humanize a bare CLI model id for the picker (`grok-4.6` → `Grok 4.6`).
fn model_display_name(id: &str) -> String {
    match id.strip_prefix("grok-") {
        Some(rest) if !rest.is_empty() => format!("Grok {rest}"),
        _ => id.to_string(),
    }
}

fn model_info(id: String, tier: i32) -> ModelInfo {
    ModelInfo {
        display_name: model_display_name(&id),
        id,
        capabilities: vec!["code".into(), "reasoning".into()],
        tier,
    }
}

fn merge_additional_models(base: Vec<ModelInfo>, extras: Vec<String>) -> Vec<ModelInfo> {
    turn::merge_additional_models(base, extras, |id| model_info(id, 99))
}

/// Run `grok models` under `env` and parse the listing. `None` on any
/// failure so the caller seeds statically.
async fn probe_cli_models(cli_path: &str, env: &HashMap<String, String>) -> Option<Vec<String>> {
    let mut cmd = Command::new(cli_path);
    cmd.arg("models")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for (key, value) in env {
        cmd.env(key, value);
    }

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("grok: model discovery spawn failed: {e}");
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
            tracing::warn!("grok: model discovery failed: {e}");
            return None;
        }
        Err(_) => {
            tracing::warn!("grok: model discovery timed out");
            return None;
        }
    };

    if !output.status.success() {
        tracing::warn!(
            "grok: model discovery exited with {:?}",
            output.status.code()
        );
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parser::parse_cli_models(&text).map(|c| c.models)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::stream::SpawnConfig;
    /// Both sign-in markers must classify as auth failures, so the crash
    /// row can offer "re-login" rather than "retry".
    #[test]
    fn sign_in_markers_are_auth_failures() {
        assert_eq!(STDERR_MARKERS[0].marker, "accounts.x.ai/oauth2/device");
        assert_eq!(STDERR_MARKERS[1].marker, "Not signed in");
        for marker in STDERR_MARKERS {
            assert_eq!(marker.kind, crate::provider::stream::CrashKind::AuthExpired);
            assert!(marker.abort);
        }
    }

    /// The composed prompt a turn ships when nothing custom is configured.
    fn working_style() -> String {
        turn::compose_system_prompt(&SpawnConfig::default())
    }

    #[test]
    fn build_args_hardens_prompt_and_sets_streaming() {
        let args = build_cli_args(
            "grok-4.5",
            "--always-approve evil",
            None,
            None,
            &working_style(),
        );
        // The whole prompt is the value of --single, never a separate flag.
        assert_eq!(args[0], "--single=--always-approve evil");
        assert!(args.contains(&"--output-format=streaming-json".to_string()));
        assert!(args.contains(&"--always-approve".to_string()));
        assert!(args.contains(&"--model=grok-4.5".to_string()));
        // No bare `--always-approve evil` token splitting out of the prompt.
        assert!(!args.iter().any(|a| a == "evil"));
    }

    /// Sessions stored before the grok 1.0 catalog change carry `grok-build`,
    /// which the CLI now rejects with "unknown model id"; it must map to the
    /// current default rather than fail every turn.
    #[test]
    fn legacy_grok_build_maps_to_current_default() {
        assert_eq!(effective_model("grok-build"), DEFAULT_MODEL);
        assert_eq!(effective_model(""), DEFAULT_MODEL);
        assert_eq!(effective_model("grok-4.5"), "grok-4.5");
    }

    #[test]
    fn build_args_includes_session_effort_and_system_prompt() {
        // A custom prompt EXTENDS the system prompt: the shared working-style
        // rules stay, then the per-spawn suffix, then the override (mirrors
        // Claude). The suffix used to be dropped entirely on grok.
        let config = SpawnConfig {
            system_prompt_suffix: Some("# Repeating Task Context".into()),
            system_prompt_override: Some("be terse".into()),
            ..Default::default()
        };
        let args = build_cli_args(
            "grok-4.5",
            "hi",
            Some("sess-7"),
            Some("high"),
            &turn::compose_system_prompt(&config),
        );
        // Resume rides `--resume`; `--session-id` would error on an
        // existing id under grok 1.0.
        assert!(args.contains(&"--resume=sess-7".to_string()));
        assert!(args.contains(&"--effort=high".to_string()));
        assert!(args.contains(&format!(
            "--system-prompt-override={}\n# Repeating Task Context\nbe terse",
            crate::provider::WORKING_STYLE
        )));
    }

    #[test]
    fn build_args_sends_working_style_when_no_override() {
        // With no override, every session still ships the shared rules.
        let args = build_cli_args("grok-4.5", "hi", None, None, &working_style());
        assert!(args.contains(&format!(
            "--system-prompt-override={}",
            crate::provider::WORKING_STYLE
        )));
    }

    #[test]
    fn build_args_omits_optional_flags_when_absent() {
        let args = build_cli_args("grok-4.5", "hi", None, Some("  "), &working_style());
        assert!(!args.iter().any(|a| a.starts_with("--resume")));
        assert!(!args.iter().any(|a| a.starts_with("--session-id")));
        // Whitespace-only effort is treated as absent.
        assert!(!args.iter().any(|a| a.starts_with("--effort")));
        // The system prompt is ALWAYS sent (falls back to the shared
        // working-style rules), so it's present even with no override.
        assert!(
            args.iter()
                .any(|a| a.starts_with("--system-prompt-override="))
        );
    }

    #[test]
    fn default_models_are_prefix_free() {
        for m in default_models() {
            assert!(!m.id.contains(':'), "id {} should be prefix-free", m.id);
            assert!(!m.id.contains('@'), "id {} should be account-free", m.id);
        }
    }

    #[test]
    fn default_models_seed_includes_46_and_45() {
        let seed = default_models();
        let ids: Vec<&str> = seed.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["grok-4.6", "grok-4.5"]);
        assert_eq!(seed[0].display_name, "Grok 4.6");
        assert_eq!(seed[1].display_name, "Grok 4.5");
    }

    #[test]
    fn model_display_name_humanizes_grok_prefix() {
        assert_eq!(model_display_name("grok-4.6"), "Grok 4.6");
        assert_eq!(model_display_name("grok-4.5"), "Grok 4.5");
        assert_eq!(model_display_name("other"), "other");
    }

    #[test]
    fn merge_additional_models_dedups_against_seed() {
        let merged = merge_additional_models(
            default_models(),
            vec!["grok-4.5".into(), "custom-x".into(), "custom-x".into()],
        );
        let ids: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["grok-4.6", "grok-4.5", "custom-x"]);
        assert_eq!(merged[2].display_name, "custom-x");
    }

    /// `dynamic_models` shells out to `cli_path models` and surfaces the
    /// parsed catalog (instead of only the static seed).
    #[tokio::test]
    async fn dynamic_models_reads_catalog_from_cli_shim() {
        use crate::plugin::settings::{FieldKind, SettingField, SettingsSchema};
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let shim = dir.path().join("fake-grok");
        std::fs::write(
            &shim,
            r#"#!/bin/sh
if [ "$1" = models ]; then
  cat <<'EOF'
You are logged in with grok.com.

Default model: grok-4.6

Available models:
  * grok-4.6 (default)
  - grok-4.5
EOF
  exit 0
fi
echo "unexpected: $*" >&2
exit 1
"#,
        )
        .unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();

        let db = crate::db::Db::in_memory().unwrap();
        db.set_plugin_setting(
            "grok",
            "cli_path",
            &serde_json::json!(shim.to_str().unwrap()),
        )
        .await
        .unwrap();

        let schema = SettingsSchema::new(vec![SettingField {
            key: "cli_path".into(),
            title: "CLI Path".into(),
            description: None,
            required: false,
            kind: FieldKind::String {
                secret: false,
                default: Some("grok".into()),
                placeholder: None,
            },
        }]);
        let store = crate::plugin::settings::PluginSettingsStore::new("grok", schema, db);
        let provider = GrokProvider::new().with_settings(store);

        let models = provider.dynamic_models().await.expect("Some");
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["grok-4.6", "grok-4.5"], "got {ids:?}");
        assert_eq!(models[0].display_name, "Grok 4.6");
    }

    /// When discovery is disabled, the picker keeps the static seed even if
    /// a working CLI is configured.
    #[tokio::test]
    async fn dynamic_models_respects_discover_models_off() {
        use crate::plugin::settings::{FieldKind, SettingField, SettingsSchema};
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let shim = dir.path().join("fake-grok");
        // Shim that would return a distinctive id if probed.
        std::fs::write(
            &shim,
            r#"#!/bin/sh
echo "Default model: probed-only"
echo
echo "Available models:"
echo "  * probed-only (default)"
"#,
        )
        .unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();

        let db = crate::db::Db::in_memory().unwrap();
        db.set_plugin_setting(
            "grok",
            "cli_path",
            &serde_json::json!(shim.to_str().unwrap()),
        )
        .await
        .unwrap();
        db.set_plugin_setting("grok", "discover_models", &serde_json::json!(false))
            .await
            .unwrap();

        let schema = SettingsSchema::new(vec![
            SettingField {
                key: "cli_path".into(),
                title: "CLI Path".into(),
                description: None,
                required: false,
                kind: FieldKind::String {
                    secret: false,
                    default: Some("grok".into()),
                    placeholder: None,
                },
            },
            SettingField {
                key: "discover_models".into(),
                title: "Auto-Discover Models".into(),
                description: None,
                required: false,
                kind: FieldKind::Boolean { default: true },
            },
        ]);
        let store = crate::plugin::settings::PluginSettingsStore::new("grok", schema, db);
        let provider = GrokProvider::new().with_settings(store);

        let models = provider.dynamic_models().await.expect("Some");
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["grok-4.6", "grok-4.5"]);
        assert!(!ids.contains(&"probed-only"));
    }
}
