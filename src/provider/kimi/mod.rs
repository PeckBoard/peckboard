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
//! rejects `--prompt` combined with `--yolo`); its built-in tool calls —
//! including the shell — render as ordinary tool rows with their real
//! results, since peckboard has no pre-execution gate on them.
//!
//! The CLI has no system-prompt flag, so — as with Cursor — the shared
//! WORKING_STYLE rules (plus any per-session override) are folded into the
//! first turn's prompt. MCP has no CLI flag either, but Kimi Code loads
//! `.kimi-code/mcp.json` from the working directory at session start, so the
//! peckboard MCP server (and user-defined servers) are wired through that
//! file per turn — see [`mcp`].
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

/// Default per-turn timeout. Kimi turns can take several tool steps, so this
/// is generous; an unauthenticated host fails in seconds instead.
const DEFAULT_TIMEOUT_SECS: u64 = turn::DEFAULT_TIMEOUT_SECS;
/// Default CLI binary name; overridable via the `cli_path` setting.
const DEFAULT_CLI: &str = "kimi";

/// Where to look for `kimi` when it isn't on the server's PATH: the official
/// installer's location. This matters because the PeckBoard server often runs
/// with a service PATH that predates the CLI install (the installer only
/// extends `~/.bashrc`).
const CLI_FALLBACK_DIRS: &[&str] = &["~/.kimi-code/bin"];

/// Resolve the `kimi` executable to spawn. Shared with the `/api/kimi-accounts`
/// login route, which spawns `<cli_path> login`.
pub(crate) fn resolve_cli_path(configured: &str) -> String {
    turn::resolve_cli_path(configured, CLI_FALLBACK_DIRS)
}
/// How long a model-discovery probe (success or failure) is cached, so the
/// picker doesn't shell out on every render.
const MODEL_DISCOVERY_TTL: Duration = Duration::from_secs(60);
/// Bound on how long the discovery subprocess may run.
const MODEL_DISCOVERY_TIMEOUT_SECS: u64 = 10;
/// kimi prints this when no model/credential is configured ("No model
/// configured. Run `kimi` and use /login to sign in, ..."). Unlike grok it
/// exits straight away rather than waiting on a device prompt, so the marker
/// only rewrites the crash reason — there is nothing to abort early.
const STDERR_MARKERS: &[StderrMarker] = &[StderrMarker {
    marker: "No model configured",
    message: "Kimi Code isn't signed in on this host. Run `kimi login` (or add a \
              provider to ~/.kimi-code/config.toml / set an API key in the plugin \
              settings), then try again.",
    kind: crate::provider::stream::CrashKind::AuthExpired,
    abort: false,
}];

/// Per-session tracking for an in-flight `kimi` turn.
struct KimiRun {
    handle: JoinHandle<()>,
    cancel: Arc<Notify>,
}

/// TTL cache for the model-discovery probe.
struct DiscoveryCache {
    fetched_at: Instant,
    models: Option<Vec<parser::CliModel>>,
}

/// `AgentProvider` backed by per-turn `kimi` invocations.
pub struct KimiProvider {
    settings: PluginSettingsStore,
    runs: Arc<Mutex<HashMap<String, KimiRun>>>,
    discovery_cache: Arc<Mutex<HashMap<String, DiscoveryCache>>>,
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
            discovery_cache: Arc::new(Mutex::new(HashMap::new())),
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

    /// One labelled variant per stored account (`<model>@<account_id>`,
    /// shown as `[Account] Model`). Each account's model set comes from ITS
    /// OWN config via `KIMI_CODE_HOME` discovery — host and account configs
    /// are separate files, so the host list can't stand in for an account's.
    /// Discovery off/failed falls back to mirroring `base` (the old shape,
    /// matching the Grok provider).
    async fn account_scoped_models(
        &self,
        base: &[ModelInfo],
        cli_path: &str,
        discover: bool,
    ) -> Vec<ModelInfo> {
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
            let discovered = match (&acct.config_dir, discover) {
                (Some(dir), true) => {
                    let env = HashMap::from([("KIMI_CODE_HOME".to_string(), dir.clone())]);
                    self.discovered_models(cli_path, &acct.id, &env).await
                }
                _ => None,
            };
            let acct_base: Vec<ModelInfo> = match discovered {
                Some(models) if !models.is_empty() => default_models()
                    .into_iter()
                    .chain(models.into_iter().map(model_info))
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
    /// Run the discovery command under `env` and return model aliases, via
    /// the per-scope TTL cache (`scope`: "" = host config, or a kimi account
    /// id whose env selects its `KIMI_CODE_HOME`). `Some(list)` on success
    /// (possibly empty), `None` when the last probe failed and the caller
    /// should fall back.
    async fn discovered_models(
        &self,
        cli_path: &str,
        scope: &str,
        env: &HashMap<String, String>,
    ) -> Option<Vec<parser::CliModel>> {
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
        let cli_path = resolve_cli_path(
            &setting_str(&settings, "cli_path").unwrap_or_else(|| DEFAULT_CLI.to_string()),
        );
        let extras = setting_str_list(&settings, "additional_models");
        let discover = setting_bool(&settings, "discover_models").unwrap_or(true);

        let base = if discover {
            match self.discovered_models(&cli_path, "", &HashMap::new()).await {
                // The config-default entry stays first so an alias-free setup
                // still has a working selection.
                Some(models) if !models.is_empty() => default_models()
                    .into_iter()
                    .chain(models.into_iter().map(model_info))
                    .collect(),
                // Discovery failed or returned nothing usable → static seed.
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
            match db.list_kimi_accounts().await {
                Ok(accounts) if !accounts.is_empty() => return Some(true),
                Ok(_) => {}
                // Can't tell — don't warn on a transient DB error.
                Err(_) => return None,
            }
        }
        if std::env::var("KIMI_API_KEY").is_ok_and(|v| !v.is_empty()) {
            return Some(true);
        }
        // Host-level login: the CLI writes tokens into ~/.kimi-code/config.toml.
        let host = dirs::home_dir()
            .map(|h| h.join(".kimi-code").join("config.toml"))
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
            conversation_id,
            completion_tx,
            // kimi runs its own tool loop; the WASM plugin tool host isn't
            // wired in as a *tool* provider. Peckboard MCP tools reach the
            // CLI via the workspace `.kimi-code/mcp.json` instead (see the
            // `mcp` module below). It IS handed to the turn harness so a
            // `todo`-hook plugin can drive lifecycle tracking off the
            // assistant text.
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
        let cli_path = resolve_cli_path(
            &setting_str(&settings, "cli_path").unwrap_or_else(|| DEFAULT_CLI.to_string()),
        );
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
            // `kimi --help` carries no image/attachment flag, so say so in
            // the transcript rather than silently answering the text alone.
            turn::notify_attachments_dropped(
                &db,
                &broadcaster,
                &session_id,
                "kimi",
                message.attachments.len(),
            )
            .await;
        }
        // Wire the peckboard MCP server (and any user-defined servers) into
        // the workspace `.kimi-code/mcp.json` — Kimi Code loads it at session
        // start; the bearer token rides an env var so the file stays
        // secret-free. Best-effort: the turn still runs without MCP on any
        // failure.
        let mcp_wiring = config.mcp_config_path.as_deref().and_then(|path| {
            if config.working_dir.is_empty() {
                return None;
            }
            let wiring = mcp::parse_worker_mcp_config(path)?;
            match mcp::ensure_workspace_mcp_config(
                &config.working_dir,
                &wiring.url,
                &wiring.extra_servers,
            ) {
                Ok(_) => Some(wiring),
                Err(e) => {
                    tracing::warn!(session_id = %session_id, "kimi: MCP wiring skipped: {e}");
                    None
                }
            }
        });
        if let Some(wiring) = &mcp_wiring {
            env.insert(mcp::TOKEN_ENV_VAR.into(), wiring.token.clone());
        }

        // No system-prompt flag: the composed prompt (shared working-style
        // rules, then any per-spawn suffix, then any per-session override)
        // rides the first turn's text, Cursor-style.
        let system_prompt = turn::compose_system_prompt(&config);

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
            let mut stream = KimiStream::default();
            let result = turn::run_turn(
                TurnSpec {
                    provider: "kimi",
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
                    stderr_markers: STDERR_MARKERS,
                    spawn_hint: Some(
                        "Install Kimi Code with `curl -fsSL \
                         https://code.kimi.com/kimi-code/install.sh | bash` or point \
                         the plugin's CLI Path setting at the binary.",
                    ),
                    empty_exit_reason: "kimi exited without a successful result",
                    // kimi's stream has no init frame to derive a Started
                    // from (system.version carries no model).
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
                    error_kind: result.error_kind,
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

/// Adapts the kimi prompt-mode stream-json parser to the shared turn harness.
#[derive(Default)]
struct KimiStream {
    conversation_id: Option<String>,
}

impl TurnStream for KimiStream {
    fn on_line(&mut self, json: &serde_json::Value) -> Vec<ProviderEvent> {
        parser::parse_stream_json(json, &mut self.conversation_id)
    }

    fn take_conversation_id(&mut self) -> Option<String> {
        self.conversation_id.take()
    }
}

/// Probe the CLI for its configured model aliases via
/// `kimi provider list --json`, run under `env` (e.g. an account's
/// `KIMI_CODE_HOME`).
async fn probe_cli_models(
    cli_path: &str,
    env: &HashMap<String, String>,
) -> Option<Vec<parser::CliModel>> {
    let mut cmd = Command::new(cli_path);
    cmd.args(["provider", "list", "--json"])
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

fn model_info(m: parser::CliModel) -> ModelInfo {
    let display_name = match &m.display_name {
        Some(d) => format!("{d} (Kimi)"),
        None => format!("{} (Kimi)", m.id),
    };
    ModelInfo {
        id: m.id,
        display_name,
        capabilities: m.capabilities,
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
    turn::merge_additional_models(base, extras, |name| {
        model_info(parser::CliModel {
            id: name,
            display_name: None,
            capabilities: vec!["code".into()],
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "No model configured" means unsigned-in, not a generic failure.
    #[test]
    fn no_model_configured_marker_is_an_auth_failure() {
        let marker = &STDERR_MARKERS[0];
        assert_eq!(marker.marker, "No model configured");
        assert_eq!(marker.kind, crate::provider::stream::CrashKind::AuthExpired);
        assert!(!marker.abort);
    }
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
    fn cli_path_falls_back_to_the_installer_location() {
        let tmp = tempfile::tempdir().unwrap();
        let path_dir = tmp.path().join("onpath");
        let home = tmp.path().join("home");
        let installer_bin = home.join(".kimi-code").join("bin");
        std::fs::create_dir_all(&path_dir).unwrap();
        std::fs::create_dir_all(&installer_bin).unwrap();
        let path_var = path_dir.to_str().unwrap().to_string();
        let home_str = home.to_str();
        let resolve = |configured: &str, path_var: &str| {
            turn::resolve_cli_path_in(configured, path_var, home_str, CLI_FALLBACK_DIRS)
        };

        // A path with a slash passes through untouched.
        assert_eq!(resolve("/opt/kimi", ""), "/opt/kimi");

        // Bare name, not on PATH, no installer file → unchanged (spawn will
        // surface the original error).
        assert_eq!(resolve("kimi", &path_var), "kimi");

        // Installer fallback exists → absolute fallback wins.
        std::fs::write(installer_bin.join("kimi"), b"#!").unwrap();
        assert_eq!(
            resolve("kimi", &path_var),
            installer_bin.join("kimi").to_string_lossy()
        );

        // On PATH → bare name kept even with the fallback present.
        std::fs::write(path_dir.join("kimi"), b"#!").unwrap();
        assert_eq!(resolve("kimi", &path_var), "kimi");
    }
    #[test]
    fn default_models_are_prefix_free() {
        for m in default_models() {
            assert!(!m.id.contains(':'), "id {} should be prefix-free", m.id);
        }
    }
}
