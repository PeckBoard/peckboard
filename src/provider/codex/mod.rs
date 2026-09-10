//! Codex CLI (`codex exec`) agent provider.
//!
//! Drives sessions through OpenAI's Codex CLI **once per turn** — the same
//! harness as Grok/Cursor/Kimi, not Claude's long-lived stdin process.
//! Argv and JSONL match `CLI.md` (captured from `openai/codex` main):
//!
//! ```text
//! codex exec --json --dangerously-bypass-approvals-and-sandbox \
//!     --skip-git-repo-check [-m MODEL] [-c model_reasoning_effort=…] \
//!     [--image PATH]… PROMPT
//! ```
//!
//! Resume puts global flags *before* `resume`, matching the sandbox flag's
//! old placement rule (not a global on the `resume` subcommand):
//!
//! ```text
//! codex exec --json --dangerously-bypass-approvals-and-sandbox \
//!     --skip-git-repo-check resume <thread_id> [--image PATH]… PROMPT
//! ```
//!
//! `thread_id` comes from the first JSONL line (`thread.started`). First
//! turn prepends the shared working-style rules (Cursor-style); resume does
//! not. Auth is host `~/.codex/auth.json` (ChatGPT sign-in via `codex login
//! --device-auth`) and/or per-account `CODEX_HOME` dirs from the Codex
//! Accounts table. API-key login is not offered.

pub mod login;
mod mcp;
mod parser;

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::process::Command;
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

use crate::plugin::settings::PluginSettingsStore;
use crate::provider::agent::{AgentProvider, ProcessCompletion, SendMessageContext};
use crate::provider::message::UserAttachment;
use crate::provider::registry::split_model_account;
use crate::provider::stream::{CrashKind, ModelInfo, ProviderEvent};
use crate::provider::turn::{
    self, StderrMarker, TurnSpec, TurnStream, setting_bool, setting_str, setting_str_list,
};

const DEFAULT_CLI: &str = "codex";
const CLI_FALLBACK_DIRS: &[&str] = turn::COMMON_CLI_FALLBACK_DIRS;
const MODEL_DISCOVERY_TTL: Duration = Duration::from_secs(60);
const API_KEY_ENV: &str = "CODEX_API_KEY";
const MODEL_DISCOVERY_TIMEOUT_SECS: u64 = 10;
const AUTH_HINT: &str = "Codex isn't signed in. Sign in with ChatGPT in Settings → Codex \
                         Accounts, or run `codex login --device-auth` on the host.";

const STDERR_MARKERS: &[StderrMarker] = &[
    StderrMarker {
        marker: "Not logged in",
        message: AUTH_HINT,
        kind: CrashKind::AuthExpired,
        abort: true,
    },
    StderrMarker {
        marker: "Not signed in",
        message: AUTH_HINT,
        kind: CrashKind::AuthExpired,
        abort: true,
    },
    StderrMarker {
        marker: "no Codex credentials were found",
        message: AUTH_HINT,
        kind: CrashKind::AuthExpired,
        abort: true,
    },
    StderrMarker {
        marker: "CODEX_API_KEY",
        message: AUTH_HINT,
        kind: CrashKind::AuthExpired,
        abort: true,
    },
];

const SPAWN_HINT: &str = "Install the Codex CLI with `curl -fsSL https://chatgpt.com/codex/install.sh | sh`, \
     or point the plugin's CLI Path setting at the binary. Docs: \
     https://learn.chatgpt.com/docs/codex/cli";

struct CodexRun {
    handle: JoinHandle<()>,
    cancel: Arc<Notify>,
    retire: Arc<Notify>,
}

struct DiscoveryCache {
    fetched_at: Instant,
    models: Option<Vec<String>>,
}

/// `AgentProvider` backed by per-turn `codex exec` invocations.
pub struct CodexProvider {
    runs: Arc<Mutex<HashMap<String, CodexRun>>>,
    /// DB handle for multi-account support: `dynamic_models` enumerates the
    /// stored accounts and `send_message` resolves the per-account `CODEX_HOME`
    /// to inject. `None` in tests / no-DB registrations keeps the
    /// single-(Default-)account behaviour.
    db: Option<crate::db::Db>,
    settings: Option<PluginSettingsStore>,
    /// Per-scope (`""` = host, else account id) cache of `codex debug models`
    /// probes.
    discovery_cache: Arc<Mutex<HashMap<String, DiscoveryCache>>>,
}

impl CodexProvider {
    pub fn new() -> Self {
        CodexProvider {
            runs: Arc::new(Mutex::new(HashMap::new())),
            db: None,
            settings: None,
            discovery_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_settings(mut self, settings: PluginSettingsStore) -> Self {
        self.settings = Some(settings);
        self
    }

    /// Attach a DB handle so the provider can resolve Codex accounts.
    pub fn with_db(mut self, db: crate::db::Db) -> Self {
        self.db = Some(db);
        self
    }

    async fn load_settings(&self) -> HashMap<String, serde_json::Value> {
        match &self.settings {
            Some(store) => match store.load().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("codex: failed to load settings: {e}");
                    HashMap::new()
                }
            },
            None => HashMap::new(),
        }
    }

    async fn cli_path(&self) -> String {
        let settings = self.load_settings().await;
        let configured = setting_str(&settings, "cli_path");
        turn::resolve_cli_path(
            &configured.unwrap_or_else(|| DEFAULT_CLI.to_string()),
            CLI_FALLBACK_DIRS,
        )
    }

    /// Resolve `account_id` to its `CODEX_HOME` so the spawned CLI runs as
    /// that ChatGPT login. An account id that no longer exists (deleted out
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
            .get_codex_account(account_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("codex account not found: {account_id}"))?;
        if let Some(dir) = &account.config_dir {
            std::fs::create_dir_all(dir).ok();
            env.insert("CODEX_HOME".into(), dir.clone());
        }
        // ChatGPT sign-in lives in auth.json; an inherited API-key env would
        // steal the session onto usage-based billing.
        env.remove(API_KEY_ENV);
        env.remove("OPENAI_API_KEY");
        Ok(())
    }

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
    /// `CODEX_HOME`. Discovery off/failed falls back to mirroring `base`.
    async fn account_scoped_models(
        &self,
        base: &[ModelInfo],
        cli_path: &str,
        discover: bool,
    ) -> Vec<ModelInfo> {
        let Some(db) = &self.db else {
            return Vec::new();
        };
        let accounts = match db.list_codex_accounts().await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!("codex: failed to list accounts for model catalog: {e}");
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
                        tracing::warn!("codex: skip discovery for account {}: {e}", acct.id);
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

impl Default for CodexProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentProvider for CodexProvider {
    fn id(&self) -> &str {
        "codex"
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
            match db.list_codex_accounts().await {
                Ok(accounts) if !accounts.is_empty() => return Some(true),
                Ok(_) => {}
                // Can't tell — don't warn on a transient DB error.
                Err(_) => return None,
            }
        }
        Some(host_auth_present())
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
            plugins,
        } = ctx;

        {
            let mut runs = self.runs.lock().await;
            if let Some(old) = runs.remove(&session_id) {
                old.cancel.notify_one();
            }
        }

        let cli_path = self.cli_path().await;
        let model = resolve_model(&config.model);

        let stripped = config
            .model
            .strip_prefix("codex:")
            .map(|m| m.to_string())
            .unwrap_or_else(|| config.model.clone());
        let (_base_model, account_id) = split_model_account(&stripped);

        let mut env = config.env.clone();
        if let Some(account_id) = account_id {
            self.inject_account_env(account_id, &mut env).await?;
        }

        let (image_paths, dropped) =
            stage_image_attachments(&config.working_dir, &message.attachments);
        if dropped > 0 {
            turn::notify_attachments_dropped(&db, &broadcaster, &session_id, "codex", dropped)
                .await;
        }

        // Codex layers a project's `.codex/config.toml` only when the project
        // is trusted, so the managed block alone can be a no-op. The `-c`
        // overrides below are the wiring we actually rely on.
        let mut mcp_overrides: Vec<String> = Vec::new();
        if !config.working_dir.is_empty()
            && let Some(path) = config.mcp_config_path.as_deref()
        {
            let wiring = mcp::parse_worker_mcp_config(path);
            let extras = match &wiring {
                Some(w) => w.extra_servers.clone(),
                None => mcp::extra_servers_from_worker_config(path),
            };
            mcp_overrides =
                mcp::cli_config_overrides(wiring.as_ref().map(|w| w.url.as_str()), &extras);
            if let Some(w) = &wiring {
                env.insert(mcp::TOKEN_ENV_VAR.to_string(), w.token.clone());
            }
            if let Err(e) = mcp::ensure_workspace_codex_toml(
                &config.working_dir,
                wiring.as_ref().map(|w| w.url.as_str()),
                &extras,
            ) {
                tracing::warn!(session_id = %session_id, "codex: MCP wiring skipped: {e}");
            }
        }

        let system_prompt = turn::compose_system_prompt(&config);
        let args = build_cli_args(
            model.as_deref().unwrap_or(""),
            &message.text,
            conversation_id.as_deref(),
            config.effort.as_deref(),
            &system_prompt,
            &image_paths,
            &mcp_overrides,
        );

        let cancel = Arc::new(Notify::new());
        let cancel_for_task = cancel.clone();
        let retire = Arc::new(Notify::new());
        let retire_for_task = retire.clone();
        let runs = self.runs.clone();
        let sid = session_id.clone();
        let model_label = config.model.clone();
        // Bare id when we resolved one; `bare_model_id` strips the prefix off
        // the raw label either way.
        let usage_model = model.clone().unwrap_or_else(|| model_label.clone());
        let working_dir = config.working_dir.clone();

        let handle = tokio::spawn(async move {
            let mut stream = CodexStream {
                model: Some(usage_model),
                ..Default::default()
            };
            let result = turn::run_turn(
                TurnSpec {
                    provider: "codex",
                    cli_path: &cli_path,
                    args: &args,
                    env: &env,
                    working_dir: &working_dir,
                    model_label: &model_label,
                    session_id: &sid,
                    db: &db,
                    broadcaster: broadcaster.as_ref(),
                    timeout_secs: None,
                    cancel: cancel_for_task,
                    retire: retire_for_task,
                    retire_grace_secs: turn::RETIRE_GRACE_SECS,
                    stderr_markers: STDERR_MARKERS,
                    spawn_hint: Some(SPAWN_HINT),
                    empty_exit_reason: "codex exited without a successful result",
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
            CodexRun {
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
            tracing::info!(session_id = %session_id, "Cancelling codex run");
            c.notify_one();
        }
    }

    async fn shutdown_after_turn(&self, session_id: &str) {
        let retire = {
            let runs = self.runs.lock().await;
            runs.get(session_id).map(|r| r.retire.clone())
        };
        if let Some(r) = retire {
            tracing::info!(session_id = %session_id, "Retiring codex run after turn");
            r.notify_one();
        }
    }

    async fn interrupt(&self, session_id: &str) {
        self.cancel(session_id).await;
    }

    async fn write_stdin(&self, _session_id: &str, _text: &str) -> bool {
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
                    "wait_for_termination timed out for codex run"
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

#[derive(Default)]
struct CodexStream {
    state: parser::TurnState,
    /// Session model id, stamped on the usage event so the row can be priced
    /// against the codex rates instead of the default fallback.
    model: Option<String>,
}

impl TurnStream for CodexStream {
    fn on_line(&mut self, json: &serde_json::Value) -> Vec<ProviderEvent> {
        parser::parse_stream_json(json, &mut self.state, self.model.as_deref())
    }

    fn take_conversation_id(&mut self) -> Option<String> {
        self.state.conversation_id.take()
    }

    fn take_error(&mut self) -> Option<String> {
        self.state.error.take()
    }
}

fn build_cli_args(
    model: &str,
    prompt: &str,
    conversation_id: Option<&str>,
    effort: Option<&str>,
    system_prompt: &str,
    image_paths: &[String],
    mcp_overrides: &[String],
) -> Vec<String> {
    // Approvals: `approval_policy=never` means "never ASK", NOT
    // "auto-approve" — codex hard-errors any call that needs approval with
    // "MCP tool call requires approval, but approval policy is never", which
    // killed EVERY peckboard MCP tool call from a codex session (read-only
    // ones included; there is no per-server pre-approval config key —
    // `mcp_servers.<name>.{tool_approval,trusted,auto_approve}` and
    // `mcp_tool_approval` are all rejected by `--strict-config`). The only
    // switch codex offers that actually clears the MCP approval path is
    // `--dangerously-bypass-approvals-and-sandbox`, which also drops codex's
    // own sandbox — so the agent's shell commands run unsandboxed, gated by
    // the session's folder scope alone.
    //
    // The flag supersedes `--sandbox` (it forces danger-full-access), so
    // passing both would just be contradictory config — `--sandbox` is gone.
    let mut args = vec![
        "exec".into(),
        "--json".into(),
        "--dangerously-bypass-approvals-and-sandbox".into(),
        "--skip-git-repo-check".into(),
    ];
    for over in mcp_overrides {
        args.push("-c".into());
        args.push(over.clone());
    }
    if !model.is_empty() && !crate::provider::is_auto_model(model) {
        args.push("-m".into());
        args.push(model.to_string());
    }
    if let Some(effort) = map_effort(effort) {
        args.push("-c".into());
        args.push(format!("model_reasoning_effort={effort}"));
    }
    if let Some(cid) = conversation_id {
        args.push("resume".into());
        args.push(cid.to_string());
    }
    for path in image_paths {
        args.push("--image".into());
        args.push(path.clone());
    }
    let prompt = match conversation_id {
        None if !system_prompt.trim().is_empty() => {
            format!("{}\n\n{}", system_prompt.trim(), prompt)
        }
        _ => prompt.to_string(),
    };
    args.push(prompt);
    args
}

/// Peckboard `max` (and Codex catalog `ultra`) map onto CLI `xhigh`.
fn map_effort(effort: Option<&str>) -> Option<String> {
    let e = effort?.trim();
    if e.is_empty() {
        return None;
    }
    Some(match e {
        "max" | "ultra" => "xhigh".into(),
        other => other.to_string(),
    })
}

fn resolve_model(raw: &str) -> Option<String> {
    let stripped = raw.strip_prefix("codex:").unwrap_or(raw);
    let (base, _account) = split_model_account(stripped);
    Some(base.to_string()).filter(|m| !crate::provider::is_auto_model(m))
}

fn host_auth_present() -> bool {
    let dir = std::env::var("CODEX_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".codex")));
    dir.map(|d| d.join("auth.json"))
        .is_some_and(|p| std::fs::metadata(&p).is_ok_and(|m| m.len() > 0))
}

fn is_image_attachment(a: &UserAttachment) -> bool {
    a.mime_type.to_ascii_lowercase().starts_with("image/")
        || crate::provider::message::mime_from_filename(&a.filename).starts_with("image/")
}

/// Write image attachments into the workspace so `--image PATH` can see
/// them (sandbox is workspace-write). Returns (staged paths, dropped count).
fn stage_image_attachments(
    working_dir: &str,
    attachments: &[UserAttachment],
) -> (Vec<String>, usize) {
    if attachments.is_empty() {
        return (Vec::new(), 0);
    }
    if working_dir.is_empty() {
        return (Vec::new(), attachments.len());
    }
    let mut paths = Vec::new();
    let mut dropped = 0usize;
    let dir = Path::new(working_dir).join(".peckboard-codex-images");
    for (i, att) in attachments.iter().enumerate() {
        if !is_image_attachment(att) {
            dropped += 1;
            continue;
        }
        if std::fs::create_dir_all(&dir).is_err() {
            dropped += 1;
            continue;
        }
        let name = safe_filename(&att.filename, i);
        let path = dir.join(name);
        match std::fs::write(&path, &att.data) {
            Ok(()) => paths.push(path.to_string_lossy().into_owned()),
            Err(_) => dropped += 1,
        }
    }
    (paths, dropped)
}

fn safe_filename(name: &str, idx: usize) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("image");
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = if cleaned.is_empty() {
        "image".to_string()
    } else {
        cleaned
    };
    format!("{idx}-{cleaned}")
}

fn model_display_name(id: &str) -> String {
    match id {
        "gpt-5.6-luna" => "GPT-5.6 Luna".into(),
        "gpt-5.6-terra" => "GPT-5.6 Terra".into(),
        "gpt-5.6-sol" => "GPT-5.6 Sol".into(),
        "gpt-6-astra" => "GPT-6 Astra".into(),
        "gpt-5.6" => "GPT-5.6".into(),
        other => other.to_string(),
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
    let extras: Vec<String> = extras
        .into_iter()
        .map(|id| id.strip_prefix("codex:").unwrap_or(&id).to_string())
        .collect();
    turn::merge_additional_models(base, extras, |id| model_info(id, 99))
}

/// Built-in seed used when discovery is off or fails. Ids are prefix-free;
/// the registry adds `codex:`.
pub fn default_models() -> Vec<ModelInfo> {
    vec![
        model_info("gpt-5.6-luna".into(), 0),
        model_info("gpt-5.6-terra".into(), 1),
        model_info("gpt-5.6-sol".into(), 2),
        model_info("gpt-6-astra".into(), 3),
        model_info("gpt-5.6".into(), 1),
    ]
}

async fn probe_cli_models(cli_path: &str, env: &HashMap<String, String>) -> Option<Vec<String>> {
    let mut cmd = Command::new(cli_path);
    cmd.args(["debug", "models", "--bundled"])
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
            tracing::warn!("codex: model discovery spawn failed: {e}");
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
            tracing::warn!("codex: model discovery failed: {e}");
            return None;
        }
        Err(_) => {
            tracing::warn!("codex: model discovery timed out");
            return None;
        }
    };

    if !output.status.success() {
        tracing::warn!(
            "codex: model discovery exited with {:?}",
            output.status.code()
        );
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parser::parse_cli_models(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::stream::SpawnConfig;

    fn working_style() -> String {
        turn::compose_system_prompt(&SpawnConfig::default())
    }

    #[test]
    fn auth_markers_are_auth_failures() {
        assert!(STDERR_MARKERS.iter().any(|m| m.marker == "Not logged in"));
        assert!(STDERR_MARKERS.iter().any(|m| m.marker == "Not signed in"));
        assert!(
            STDERR_MARKERS
                .iter()
                .any(|m| m.marker == "no Codex credentials were found")
        );
        assert!(STDERR_MARKERS.iter().any(|m| m.marker == "CODEX_API_KEY"));
        for marker in STDERR_MARKERS {
            assert_eq!(marker.kind, CrashKind::AuthExpired);
            assert!(marker.abort);
        }
    }

    #[test]
    fn build_args_sets_json_bypass_skip_git_and_model() {
        let args = build_cli_args(
            "gpt-5.6-terra",
            "hello",
            None,
            Some("high"),
            &working_style(),
            &[],
            &[],
        );
        assert_eq!(args[0], "exec");
        assert!(args.contains(&"--json".into()));
        // The bypass flag replaces `--sandbox` + `approval_policy=never`:
        // `never` hard-errors every MCP tool call that wants approval.
        assert!(args.contains(&"--dangerously-bypass-approvals-and-sandbox".into()));
        assert!(!args.contains(&"--sandbox".into()));
        assert!(!args.iter().any(|a| a.contains("approval_policy")));
        assert!(args.contains(&"--skip-git-repo-check".into()));
        assert!(args.contains(&"gpt-5.6-terra".into()));
        assert!(args.contains(&"model_reasoning_effort=high".into()));
        assert!(!args.contains(&"resume".into()));
        assert!(args.last().unwrap().ends_with("hello"));
    }

    #[test]
    fn max_effort_maps_to_xhigh() {
        let args = build_cli_args("gpt-5.6-luna", "hi", None, Some("max"), "", &[], &[]);
        assert!(args.contains(&"model_reasoning_effort=xhigh".into()));
        assert!(!args.iter().any(|a| a.contains("=max")));
    }
    #[test]
    fn mcp_overrides_are_passed_as_c_flags_before_resume() {
        let overrides = vec![
            "mcp_servers.peckboard.url=\"http://127.0.0.1:4100/mcp\"".to_string(),
            "mcp_servers.peckboard.enabled=true".to_string(),
        ];
        let args = build_cli_args("auto", "hi", Some("thread-9"), None, "", &[], &overrides);
        let resume_at = args.iter().position(|a| a == "resume").unwrap();
        for over in &overrides {
            let at = args.iter().position(|a| a == over).unwrap();
            assert_eq!(args[at - 1], "-c");
            assert!(at < resume_at, "-c overrides must precede resume");
        }
    }

    #[test]
    fn resume_puts_global_flags_before_resume_and_skips_working_style() {
        let args = build_cli_args(
            "gpt-6-astra",
            "follow up",
            Some("0199a213-81c0-7800-8aa1-bbab2a035a53"),
            Some("low"),
            crate::provider::WORKING_STYLE,
            &[],
            &[],
        );
        let resume_at = args.iter().position(|a| a == "resume").unwrap();
        assert_eq!(args[resume_at + 1], "0199a213-81c0-7800-8aa1-bbab2a035a53");
        let bypass_at = args
            .iter()
            .position(|a| a == "--dangerously-bypass-approvals-and-sandbox")
            .unwrap();
        assert!(bypass_at < resume_at, "global flags must precede resume");
        assert_eq!(args.last().unwrap(), "follow up");
        assert!(!args.iter().any(|a| a.contains("# Working style")));
    }

    #[test]
    fn first_turn_prepends_working_style_rules_but_resume_does_not() {
        let first = build_cli_args(
            "auto",
            "do it",
            None,
            None,
            crate::provider::WORKING_STYLE,
            &[],
            &[],
        );
        let prompt = first.last().unwrap();
        assert!(prompt.contains("# Working style"));
        assert!(prompt.ends_with("do it"));
        assert!(!first.contains(&"-m".into()));

        let resume = build_cli_args(
            "auto",
            "do it",
            Some("thread-7"),
            None,
            crate::provider::WORKING_STYLE,
            &[],
            &[],
        );
        assert_eq!(resume.last().unwrap(), "do it");
    }

    #[test]
    fn build_args_includes_image_flags() {
        let args = build_cli_args(
            "gpt-5.6",
            "see",
            None,
            None,
            "",
            &["/tmp/a.png".into(), "/tmp/b.jpg".into()],
            &[],
        );
        let images: Vec<_> = args
            .windows(2)
            .filter(|w| w[0] == "--image")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(images, vec!["/tmp/a.png", "/tmp/b.jpg"]);
    }

    #[test]
    fn cli_path_falls_back_to_local_bin() {
        let tmp = tempfile::tempdir().unwrap();
        let path_dir = tmp.path().join("onpath");
        let home = tmp.path().join("home");
        let local_bin = home.join(".local").join("bin");
        std::fs::create_dir_all(&path_dir).unwrap();
        std::fs::create_dir_all(&local_bin).unwrap();
        let path_var = path_dir.to_str().unwrap().to_string();
        let home_str = home.to_str();
        let resolve = |configured: &str, path_var: &str| {
            turn::resolve_cli_path_in(configured, path_var, home_str, CLI_FALLBACK_DIRS)
        };

        assert_eq!(resolve("/opt/codex", ""), "/opt/codex");
        assert_eq!(resolve("codex", &path_var), "codex");

        std::fs::write(local_bin.join("codex"), b"#!").unwrap();
        assert_eq!(
            resolve("codex", &path_var),
            local_bin.join("codex").to_string_lossy()
        );

        std::fs::write(path_dir.join("codex"), b"#!").unwrap();
        assert_eq!(resolve("codex", &path_var), "codex");
    }

    #[test]
    fn default_models_are_prefix_free_with_seed_tiers() {
        let seed = default_models();
        let ids: Vec<&str> = seed.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "gpt-5.6-luna",
                "gpt-5.6-terra",
                "gpt-5.6-sol",
                "gpt-6-astra",
                "gpt-5.6",
            ]
        );
        assert_eq!(seed[0].tier, 0);
        assert_eq!(seed[1].tier, 1);
        assert_eq!(seed[2].tier, 2);
        assert_eq!(seed[3].tier, 3);
        for m in &seed {
            assert!(!m.id.contains(':'), "id {} should be prefix-free", m.id);
        }
    }

    #[test]
    fn merge_additional_models_dedups_against_seed() {
        let merged = merge_additional_models(
            default_models(),
            vec![
                "gpt-5.6-luna".into(),
                "codex:gpt-5.5".into(),
                "gpt-5.5".into(),
            ],
        );
        let ids: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"gpt-5.6-luna"));
        assert!(ids.contains(&"gpt-5.5"));
        assert_eq!(ids.iter().filter(|id| **id == "gpt-5.5").count(), 1);
        assert_eq!(ids.iter().filter(|id| **id == "gpt-5.6-luna").count(), 1);
    }

    #[test]
    fn resolve_model_strips_prefix() {
        assert_eq!(
            resolve_model("codex:gpt-5.6-terra").as_deref(),
            Some("gpt-5.6-terra")
        );
        assert_eq!(resolve_model("gpt-6-astra").as_deref(), Some("gpt-6-astra"));
        assert!(resolve_model("codex:auto").is_none());
        assert!(resolve_model("auto").is_none());
    }
}
