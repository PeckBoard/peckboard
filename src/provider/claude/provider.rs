use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{Mutex, Notify, mpsc};

use crate::provider::agent::{AgentProvider, ProcessCompletion, SendMessageContext};
use crate::provider::registry::{ProviderInfo, ProviderRegistry};

use super::process::{self, LoopState, StdinMsg};

/// Upper bound on delivering one message into a run's stdin channel.
/// The channel only backs up when the stream loop stops consuming, so a
/// timeout here is a real fault to surface, not normal backpressure.
const STDIN_SEND_TIMEOUT: Duration = Duration::from_secs(5);
/// Per-session tracking entry for a running Claude CLI invocation.
///
/// The Claude CLI is spawned ONCE per session and persists across
/// turns; the streaming task owns the `Child`, this struct just
/// carries the handles needed to talk to it. To stop the child we
/// notify `cancel`; the stream loop calls `start_kill` and synthesises
/// a Crashed event so the orchestrator sees a normal completion.
struct ClaudeRun {
    cancel: Arc<Notify>,
    stdin_tx: mpsc::Sender<StdinMsg>,
    /// True while a user turn is in flight (between `send_message`
    /// writing a user envelope and the CLI emitting `result`). Read
    /// from outside the loop by `is_running` and the idle reaper.
    turn_active: Arc<AtomicBool>,
    /// Epoch ms of the last activity (event from the CLI or write to
    /// stdin). Used by the idle reaper to decide whether to recycle
    /// a quiet child.
    last_activity: Arc<AtomicU64>,
    /// Which stored Claude account (`@<account_id>` model suffix) this
    /// child authenticated as; `None` is the Default/host account.
    /// Checked on reuse in `send_message` so a turn is never written
    /// into a child billing a different account.
    account_id: Option<String>,
}

/// TTL cache for the CLI model-discovery probe. Success and failure are
/// cached alike so a broken or slow `claude` binary stalls at most one
/// model-list request per [`super::MODEL_DISCOVERY_TTL`] window.
struct DiscoveryCache {
    fetched_at: std::time::Instant,
    models: Option<Vec<crate::provider::stream::ModelInfo>>,
}

/// `AgentProvider` impl backed by the Claude CLI in stream-json
/// duplex mode.
///
/// Owns one long-lived child process per session. The first
/// `send_message` for a session spawns it and writes the initial
/// user envelope to stdin; subsequent messages — including those
/// that arrive while a turn is still in flight — write straight to
/// stdin and the CLI consumes them after the current turn finishes.
/// That's the mid-stream injection contract: there is no
/// peckboard-level queue, the CLI itself is the queue.
///
/// The dispatcher delegates here once it has resolved the model
/// prefix to `"claude"`.
pub struct ClaudeProvider {
    runs: Arc<Mutex<HashMap<String, ClaudeRun>>>,
    /// DB handle for multi-account support: `dynamic_models` enumerates the
    /// stored accounts and `send_message` resolves the per-account
    /// credential to inject. `None` in tests / no-DB registrations, which
    /// keeps the single-(Default-)account behaviour.
    db: Option<crate::db::Db>,
    /// TTL cache for the CLI-probed model catalog (see `discovered_models`).
    discovery_cache: Arc<Mutex<Option<DiscoveryCache>>>,
}

impl ClaudeProvider {
    pub fn new() -> Self {
        ClaudeProvider {
            runs: Arc::new(Mutex::new(HashMap::new())),
            db: None,
            discovery_cache: Arc::new(Mutex::new(None)),
        }
    }

    /// Attach a DB handle so the provider can resolve Claude accounts.
    /// The `claude-code` builtin wires this from its init context.
    pub fn with_db(mut self, db: crate::db::Db) -> Self {
        self.db = Some(db);
        self
    }

    /// Start the idle-process reaper as a background task.
    ///
    /// Walks the run map every `tick` and kills any child that has
    /// no turn in flight AND has been silent for at least `idle_ms`.
    /// Killing notifies the run's cancel; the stream loop tears
    /// the child down, removes its row from the map, and the next
    /// `send_message` respawns with `--resume <conv_id>` so the
    /// conversation continues seamlessly.
    ///
    /// Without the reaper, a panel with N sessions left open
    /// overnight would keep N `claude` subprocesses alive forever.
    pub fn spawn_idle_reaper(self: &Arc<Self>, idle_ms: u64, tick: Duration) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(tick);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let Some(provider) = weak.upgrade() else {
                    break;
                };
                provider.sweep_idle(idle_ms).await;
            }
        });
    }

    async fn sweep_idle(&self, idle_ms: u64) {
        let now = now_ms();
        let to_recycle: Vec<(String, mpsc::Sender<StdinMsg>, Arc<Notify>)> = {
            let runs = self.runs.lock().await;
            runs.iter()
                .filter(|(_, r)| {
                    !r.turn_active.load(Ordering::Acquire)
                        && now.saturating_sub(r.last_activity.load(Ordering::Acquire)) >= idle_ms
                })
                .map(|(sid, r)| (sid.clone(), r.stdin_tx.clone(), r.cancel.clone()))
                .collect()
        };
        for (sid, stdin_tx, cancel) in to_recycle {
            tracing::info!(session_id = %sid, "Idle reaper recycling stale claude run");
            // Graceful shutdown, NOT the user-cancel signal: cancel makes
            // the stream loop report Crashed{interrupted}/completed:false,
            // which the UI shows as a crash and the worker orchestrator
            // counts toward crash-loop auto-pause. ShutdownAfterTurn with
            // no turn in flight closes stdin, the CLI exits on EOF, and
            // the loop's exit decision stays silent — the documented
            // idle-reap behavior.
            if stdin_tx.try_send(StdinMsg::ShutdownAfterTurn).is_err() {
                // Channel full/closed — the run is wedged or already
                // winding down; fall back to the hard kill.
                cancel.notify_one();
            }
        }
    }

    /// Resolve `account_id` to its credential and add the env the spawned
    /// `claude` CLI needs to authenticate as that account: an `api_key`
    /// account injects `ANTHROPIC_API_KEY`, an `oauth_token` account injects
    /// `CLAUDE_CODE_OAUTH_TOKEN`. Both also get an isolated
    /// `CLAUDE_CONFIG_DIR` so accounts don't share local CLI state. An
    /// account id that no longer exists (deleted out from under a live
    /// session) is a hard error rather than a silent fall back to the
    /// Default/host credentials — a turn must never bill the wrong account.
    async fn inject_account_env(
        &self,
        account_id: &str,
        env: &mut HashMap<String, String>,
    ) -> anyhow::Result<()> {
        let Some(db) = &self.db else {
            return Ok(());
        };
        let account = db
            .get_claude_account(account_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("claude account not found: {account_id}"))?;
        match account.kind.as_str() {
            "api_key" => {
                env.insert("ANTHROPIC_API_KEY".into(), account.credential.clone());
            }
            "oauth_token" => {
                // Short-lived browser-login tokens are renewed here so the
                // spawned CLI never starts with an expired credential.
                let token = super::token_refresh::fresh_credential(db, &account).await?;
                env.insert("CLAUDE_CODE_OAUTH_TOKEN".into(), token);
            }
            other => return Err(anyhow::anyhow!("unknown claude account kind: {other}")),
        }
        if let Some(dir) = &account.config_dir {
            std::fs::create_dir_all(dir).ok();
            env.insert("CLAUDE_CONFIG_DIR".into(), dir.clone());
        }
        Ok(())
    }

    /// CLI-probed base catalog through the TTL cache. `None` when discovery
    /// is disabled (`PECKBOARD_CLAUDE_MODEL_DISCOVERY=0`) or the last probe
    /// failed — the caller then seeds from the static list. Failures are
    /// cached for the full TTL too, so a missing/broken CLI costs one probe
    /// timeout per window, not one per model-list request.
    async fn discovered_models(&self) -> Option<Vec<crate::provider::stream::ModelInfo>> {
        if !super::model_discovery_enabled() {
            return None;
        }
        {
            let cache = self.discovery_cache.lock().await;
            if let Some(entry) = cache.as_ref()
                && entry.fetched_at.elapsed() < super::MODEL_DISCOVERY_TTL
            {
                return entry.models.clone();
            }
        }
        let result = super::probe_cli_models().await;
        let mut cache = self.discovery_cache.lock().await;
        *cache = Some(DiscoveryCache {
            fetched_at: std::time::Instant::now(),
            models: result.clone(),
        });
        result
    }

    /// Fill the discovery cache ahead of the first model-list request — the
    /// `claude-code` builtin calls this from a background task at init so
    /// the first model-picker open never waits on the CLI spawn.
    pub async fn prime_model_cache(&self) {
        let _ = self.discovered_models().await;
    }

    /// The model catalog the picker shows: the base models (CLI-probed when
    /// possible, static seed otherwise — bare ids, Default-account) plus one
    /// labelled variant per stored account (`<model>@<account_id>`, shown as
    /// `[Account] Model`). Returns just the base list when there are no
    /// accounts or no DB handle.
    async fn account_scoped_models(&self) -> Vec<crate::provider::stream::ModelInfo> {
        let base = match self.discovered_models().await {
            Some(models) if !models.is_empty() => models,
            _ => super::discover_models(),
        };
        let Some(db) = &self.db else {
            return base;
        };
        let accounts = match db.list_claude_accounts().await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!("claude: failed to list accounts for model catalog: {e}");
                return base;
            }
        };
        if accounts.is_empty() {
            return base;
        }
        let mut out = base.clone();
        for acct in &accounts {
            for m in &base {
                out.push(crate::provider::stream::ModelInfo {
                    id: format!("{}@{}", m.id, acct.id),
                    display_name: format!("[{}] {}", acct.name, m.display_name),
                    capabilities: m.capabilities.clone(),
                    tier: m.tier,
                });
            }
        }
        out
    }
}

impl Default for ClaudeProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentProvider for ClaudeProvider {
    fn id(&self) -> &str {
        "claude"
    }

    async fn dynamic_models(&self) -> Option<Vec<crate::provider::stream::ModelInfo>> {
        Some(self.account_scoped_models().await)
    }

    fn model_price(&self, model_id: &str) -> Option<(f64, f64)> {
        crate::routes::usage::cost::known_rates_for(model_id)
            .map(|r| (r.input_per_mtok, r.output_per_mtok))
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
            // Claude parses its own TodoWrite calls into `todo` events
            // (see process.rs); it has no need for the plugin todo path.
            plugins: _,
        } = ctx;

        // Strip the `claude:` prefix if present so the CLI sees the bare
        // model id (e.g. `claude-opus-4-7`), then peel off any
        // `@<account_id>` suffix and resolve it to the credential env the
        // CLI authenticates with. A model with no suffix is the implicit
        // Default account: nothing injected, host credentials apply.
        let stripped = config
            .model
            .strip_prefix("claude:")
            .map(|m| m.to_string())
            .unwrap_or_else(|| config.model.clone());
        let (base_model, account_id) = crate::provider::registry::split_model_account(&stripped);
        let base_model = base_model.to_string();

        let mut env = config.env.clone();
        if let Some(account_id) = account_id {
            self.inject_account_env(account_id, &mut env).await?;
        }

        let cli_config = crate::provider::stream::SpawnConfig {
            model: base_model,
            env,
            ..config
        };

        // Never reuse a live child that authenticated as a DIFFERENT
        // account. Normally the handover machinery recycles the child
        // before the account flips, but two paths bypass it: the
        // begin_handover failure fallback (direct model write), and a
        // cross-account switch on a session with no history yet. Writing
        // this turn into the old child would bill the previous account —
        // wind it down and spawn fresh under the right credentials.
        let account_mismatch = {
            let runs = self.runs.lock().await;
            runs.get(&session_id)
                .is_some_and(|r| r.account_id.as_deref() != account_id)
        };
        let conversation_id = if account_mismatch {
            tracing::warn!(
                session_id = %session_id,
                account = account_id.unwrap_or("default"),
                "Live claude child belongs to a different account; recycling before dispatch"
            );
            self.cancel(&session_id).await;
            self.wait_for_termination(&session_id).await;
            // The old conversation lives under the previous account's
            // CLAUDE_CONFIG_DIR — `--resume` under the new credentials
            // would fail the spawn outright. Start a fresh conversation;
            // the next agent-start records the new id on the session row.
            None
        } else {
            conversation_id
        };

        // Lock the runs map ONCE, then either reuse the existing run's
        // stdin or spawn a new child and insert. The lock spans the
        // is-present check + insert so two concurrent first-sends for
        // the same session can't both spawn.
        let stdin_tx = {
            let mut runs = self.runs.lock().await;

            if let Some(existing) = runs.get(&session_id) {
                existing.stdin_tx.clone()
            } else {
                let process =
                    process::spawn_claude(&session_id, &cli_config, conversation_id.as_deref())?;

                let (tx, rx) = mpsc::channel::<StdinMsg>(64);
                let cancel = Arc::new(Notify::new());
                let turn_active = Arc::new(AtomicBool::new(false));
                let last_activity = Arc::new(AtomicU64::new(now_ms()));

                let run = ClaudeRun {
                    cancel: cancel.clone(),
                    stdin_tx: tx.clone(),
                    turn_active: turn_active.clone(),
                    last_activity: last_activity.clone(),
                    account_id: account_id.map(str::to_string),
                };
                runs.insert(session_id.clone(), run);

                let allowed_dir = cli_config.working_dir.clone();
                let runs_arc = self.runs.clone();
                let sid = session_id.clone();
                let completion_tx_clone = completion_tx.clone();
                let state = LoopState {
                    turn_active,
                    last_activity,
                    turn_timeout: cli_config.timeout_ms.map(Duration::from_millis),
                    interrupt_grace: process::INTERRUPT_GRACE,
                };
                tokio::spawn(async move {
                    let outcome = process::stream_events(
                        process,
                        db,
                        broadcaster,
                        rx,
                        allowed_dir,
                        cancel,
                        state,
                    )
                    .await;

                    runs_arc.lock().await.remove(&sid);

                    tracing::debug!(
                        session_id = %sid,
                        "Claude stream task finished, run removed from manager"
                    );

                    let _ = completion_tx_clone
                        .send(ProcessCompletion {
                            session_id: sid,
                            completed: outcome.completed,
                            error: outcome.error,
                            error_kind: outcome.error_kind,
                        })
                        .await;
                });

                tx
            }
        };

        // Dispatch the user turn. Errors here mean the stream task
        // has already shut down (channel closed) — return the error
        // so the caller can append a Crashed and let the user retry.
        // The `UserMessage` carries any attachments; the stream loop
        // builds the multimodal envelope in `build_user_message_frame`.
        stdin_tx
            .send(StdinMsg::UserTurn(message))
            .await
            .map_err(|e| anyhow::anyhow!("stdin channel closed: {e}"))?;

        Ok(())
    }

    async fn cancel(&self, session_id: &str) {
        let cancel = {
            let runs = self.runs.lock().await;
            runs.get(session_id).map(|r| r.cancel.clone())
        };
        match cancel {
            Some(c) => {
                tracing::info!(session_id = %session_id, "Cancelling claude run");
                c.notify_one();
            }
            None => {
                tracing::debug!(
                    session_id = %session_id,
                    "No tracked claude run to cancel (may have already exited)"
                );
            }
        }
    }

    async fn shutdown_after_turn(&self, session_id: &str) {
        // Send the rendezvous signal to the stream loop's stdin channel.
        // The loop sets a flag and breaks out *after* the next `result`
        // event, dropping the child's stdin so it exits cleanly — no
        // synthetic Crashed event, no race with the in-flight tool
        // response. If no run is tracked (already exited), nothing to do.
        let stdin_tx = {
            let runs = self.runs.lock().await;
            runs.get(session_id).map(|r| r.stdin_tx.clone())
        };
        let Some(tx) = stdin_tx else {
            tracing::debug!(
                session_id = %session_id,
                "No tracked claude run to shutdown gracefully (may have already exited)"
            );
            return;
        };
        if let Err(e) = tx.send(StdinMsg::ShutdownAfterTurn).await {
            tracing::warn!(
                session_id = %session_id,
                "Failed to send ShutdownAfterTurn to stream loop: {e}"
            );
        } else {
            tracing::info!(
                session_id = %session_id,
                "Scheduled graceful claude shutdown after current turn"
            );
        }
    }

    async fn interrupt(&self, session_id: &str) {
        // Prefer the CLI's in-band `control_request{subtype:"interrupt"}`
        // (stream-json mode): the turn settles with a normal `result` —
        // usage recorded, child kept alive for the next turn — instead of
        // paying a kill plus a `--resume` respawn. The stream loop owns
        // the grace timer and the hard-kill fallback (unsupported CLI,
        // no result within grace); this method only falls back directly
        // when the loop is unreachable.
        let stdin_tx = {
            let runs = self.runs.lock().await;
            runs.get(session_id).map(|r| r.stdin_tx.clone())
        };
        let Some(tx) = stdin_tx else {
            tracing::debug!(
                session_id = %session_id,
                "No tracked claude run to interrupt (may have already exited)"
            );
            return;
        };
        let send = tokio::time::timeout(STDIN_SEND_TIMEOUT, tx.send(StdinMsg::Interrupt)).await;
        if !matches!(send, Ok(Ok(()))) {
            tracing::warn!(
                session_id = %session_id,
                "Could not deliver in-band interrupt; falling back to kill"
            );
            self.cancel(session_id).await;
        }
    }

    async fn write_stdin(&self, session_id: &str, text: &str) -> bool {
        let tx = {
            let runs = self.runs.lock().await;
            runs.get(session_id).map(|r| r.stdin_tx.clone())
        };
        let Some(tx) = tx else {
            tracing::debug!(
                session_id = %session_id,
                "No stdin channel for claude session (process may have exited)"
            );
            return false;
        };
        // Awaited send with a timeout instead of `try_send`: control
        // responses and question answers must not be dropped just because
        // the channel is momentarily full. A timeout means the stream
        // loop stopped consuming — surface that as a failure.
        match tokio::time::timeout(
            STDIN_SEND_TIMEOUT,
            tx.send(StdinMsg::RawLine(text.to_string())),
        )
        .await
        {
            Ok(Ok(())) => {
                tracing::info!(
                    session_id = %session_id,
                    "Sent raw stdin message to claude process"
                );
                true
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    session_id = %session_id,
                    "Failed to send stdin message (stream loop gone): {e}"
                );
                false
            }
            Err(_) => {
                tracing::warn!(
                    session_id = %session_id,
                    timeout = ?STDIN_SEND_TIMEOUT,
                    "Timed out sending stdin message; stream loop not consuming"
                );
                false
            }
        }
    }

    async fn is_running(&self, session_id: &str) -> bool {
        let runs = self.runs.lock().await;
        runs.get(session_id)
            .map(|r| r.turn_active.load(Ordering::Acquire))
            .unwrap_or(false)
    }

    async fn wait_for_termination(&self, session_id: &str) {
        // The stream task removes its run from `self.runs` only AFTER
        // emitting any synthetic Crashed event and before sending the
        // ProcessCompletion. So "no longer in the map" is the signal
        // that all per-session events have hit the DB + broadcaster.
        //
        // 10s upper bound is generous — `start_kill` plus the CLI's
        // tear-down is normally milliseconds; if we hit it, something
        // is genuinely wedged and the caller is better off proceeding
        // than blocking forever.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if !self.runs.lock().await.contains_key(session_id) {
                return;
            }
            if std::time::Instant::now() >= deadline {
                tracing::warn!(
                    session_id = %session_id,
                    "wait_for_termination timed out; claude run may still be winding down"
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn supports_mid_stream_injection(&self) -> bool {
        // The CLI in stream-json mode reads user envelopes from
        // stdin at any time and consumes them after the current
        // `result`. Concurrent dispatches share the same long-
        // lived child.
        true
    }

    async fn cleanup(&self) {
        // The stream task removes itself from the map on completion,
        // so there is nothing to sweep here. Kept as a no-op for API
        // parity.
    }

    async fn shutdown(&self) {
        let entries: Vec<(String, Arc<Notify>)> = {
            let runs = self.runs.lock().await;
            runs.iter()
                .map(|(sid, r)| (sid.clone(), r.cancel.clone()))
                .collect()
        };
        if entries.is_empty() {
            return;
        }

        tracing::info!("Shutting down {} running claude run(s)", entries.len());
        for (session_id, cancel) in entries {
            tracing::info!(session_id = %session_id, "Notifying claude run to shut down");
            cancel.notify_one();
        }
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::timeout;

    /// Insert a fake `ClaudeRun` for `session_id` without spawning a real
    /// child process. Same-module access lets the test fixture build the
    /// run struct directly. Returns the receiver end of the stdin channel
    /// so the test can observe what the provider writes to it.
    async fn install_fake_run(
        provider: &ClaudeProvider,
        session_id: &str,
    ) -> mpsc::Receiver<super::super::process::StdinMsg> {
        let (tx, rx) = mpsc::channel::<super::super::process::StdinMsg>(8);
        let run = ClaudeRun {
            cancel: Arc::new(Notify::new()),
            stdin_tx: tx,
            turn_active: Arc::new(AtomicBool::new(true)),
            last_activity: Arc::new(AtomicU64::new(now_ms())),
            account_id: None,
        };
        provider
            .runs
            .lock()
            .await
            .insert(session_id.to_string(), run);
        rx
    }

    /// Minimal account row for the credential-injection tests.
    fn account(
        id: &str,
        kind: &str,
        credential: &str,
        config_dir: Option<&str>,
    ) -> crate::db::models::NewClaudeAccount {
        crate::db::models::NewClaudeAccount {
            id: id.into(),
            name: id.into(),
            kind: kind.into(),
            credential: credential.into(),
            config_dir: config_dir.map(str::to_string),
            budget_window_hours: None,
            budget_limit_usd: None,
            budget_limit_tokens: None,
            warn_threshold: 0.75,
            critical_threshold: 0.95,
            created_at: 0,
            updated_at: 0,
            refresh_token: None,
            token_expires_at: None,
        }
    }

    /// The credential seam behind every spawn AND resume: an `api_key`
    /// account must surface as ANTHROPIC_API_KEY, an `oauth_token`
    /// account as CLAUDE_CODE_OAUTH_TOKEN plus its isolated
    /// CLAUDE_CONFIG_DIR — the dir the CLI also resumes conversations
    /// from, so a resumed session lands back on the same account state.
    #[tokio::test]
    async fn inject_account_env_resolves_kind_and_config_dir() {
        let db = crate::db::Db::in_memory().unwrap();
        db.create_claude_account(account("acc_key", "api_key", "sk-test", None))
            .await
            .unwrap();
        let tmp = std::env::temp_dir().join("pb-test-acc-oauth");
        db.create_claude_account(account(
            "acc_oauth",
            "oauth_token",
            "tok-test",
            Some(tmp.to_str().unwrap()),
        ))
        .await
        .unwrap();
        let provider = ClaudeProvider::new().with_db(db);

        let mut env = HashMap::new();
        provider
            .inject_account_env("acc_key", &mut env)
            .await
            .unwrap();
        assert_eq!(
            env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-test")
        );
        assert!(!env.contains_key("CLAUDE_CODE_OAUTH_TOKEN"));

        let mut env = HashMap::new();
        provider
            .inject_account_env("acc_oauth", &mut env)
            .await
            .unwrap();
        assert_eq!(
            env.get("CLAUDE_CODE_OAUTH_TOKEN").map(String::as_str),
            Some("tok-test")
        );
        assert_eq!(
            env.get("CLAUDE_CONFIG_DIR").map(String::as_str),
            tmp.to_str()
        );
    }

    /// An account id that no longer resolves must be a hard error — never
    /// a silent fallback to the Default/host credentials.
    #[tokio::test]
    async fn inject_account_env_missing_account_is_hard_error() {
        let db = crate::db::Db::in_memory().unwrap();
        let provider = ClaudeProvider::new().with_db(db);
        let mut env = HashMap::new();
        let res = provider.inject_account_env("acc_gone", &mut env).await;
        assert!(
            res.is_err(),
            "missing account must not fall back to host credentials"
        );
        assert!(env.is_empty());
    }
    #[tokio::test]
    async fn shutdown_after_turn_sends_message_to_stream_loop() {
        // The provider must NOT touch the cancel signal — the whole
        // point of the graceful path is to let the in-flight turn
        // finish. The only side-effect should be a single
        // `ShutdownAfterTurn` message on the run's stdin channel.
        let provider = ClaudeProvider::new();
        let mut rx = install_fake_run(&provider, "s1").await;

        provider.shutdown_after_turn("s1").await;

        let received = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("shutdown_after_turn should send a message")
            .expect("channel must yield a message");
        assert!(
            matches!(received, super::super::process::StdinMsg::ShutdownAfterTurn),
            "expected ShutdownAfterTurn, got something else"
        );
    }

    #[tokio::test]
    async fn shutdown_after_turn_is_noop_for_unknown_session() {
        // No tracked run → no panic, no error. The MCP handler fires
        // this fire-and-forget for every dispatched session id, so
        // hitting one that already exited must be silent.
        let provider = ClaudeProvider::new();
        // Just make sure this returns without panicking.
        provider.shutdown_after_turn("unknown-session").await;
    }

    #[tokio::test]
    async fn shutdown_after_turn_does_not_fire_cancel() {
        // Independent verification that the graceful path is separate
        // from the hard-kill path: the cancel Notify on the run must
        // not be triggered. We park a waiter on it and confirm it
        // never fires within a reasonable window.
        let provider = ClaudeProvider::new();
        let (tx, _rx) = mpsc::channel::<super::super::process::StdinMsg>(8);
        let cancel = Arc::new(Notify::new());
        let run = ClaudeRun {
            cancel: cancel.clone(),
            stdin_tx: tx,
            turn_active: Arc::new(AtomicBool::new(true)),
            last_activity: Arc::new(AtomicU64::new(now_ms())),
            account_id: None,
        };
        provider.runs.lock().await.insert("s2".into(), run);

        provider.shutdown_after_turn("s2").await;

        // 200ms is plenty for any spurious notify to land; an
        // intentional cancel would fire synchronously.
        let cancel_fired = timeout(Duration::from_millis(200), cancel.notified())
            .await
            .is_ok();
        assert!(
            !cancel_fired,
            "shutdown_after_turn must not trigger the hard-cancel signal"
        );
    }

    /// In-band interrupt: `interrupt` must deliver `StdinMsg::Interrupt`
    /// to the stream loop instead of killing the child via cancel.
    #[tokio::test]
    async fn interrupt_sends_in_band_message_to_stream_loop() {
        let provider = ClaudeProvider::new();
        let mut rx = install_fake_run(&provider, "s-int").await;
        provider.interrupt("s-int").await;
        let msg = rx.try_recv().expect("interrupt message delivered");
        assert!(matches!(msg, StdinMsg::Interrupt));
    }

    #[tokio::test]
    async fn interrupt_without_run_is_noop() {
        let provider = ClaudeProvider::new();
        // No tracked run — must return without panicking or blocking.
        provider.interrupt("missing").await;
    }

    /// `write_stdin` must apply backpressure (awaited send + timeout)
    /// instead of silently dropping on a full channel.
    #[tokio::test(start_paused = true)]
    async fn write_stdin_reports_failure_when_loop_stops_consuming() {
        let provider = ClaudeProvider::new();
        let mut rx = install_fake_run(&provider, "s-full").await;
        // Fill the fake run's channel with no consumer.
        {
            let runs = provider.runs.lock().await;
            let tx = runs.get("s-full").unwrap().stdin_tx.clone();
            while tx.try_send(StdinMsg::RawLine("x".into())).is_ok() {}
        }
        // Paused clock: the awaited send parks, the timeout auto-advances.
        assert!(!provider.write_stdin("s-full", "{}").await);
        // A drained channel accepts again.
        while rx.try_recv().is_ok() {}
        assert!(provider.write_stdin("s-full", "{}").await);
    }
}

/// Register the Claude CLI provider in the registry and start the
/// idle-process reaper.
///
/// `idle_ms` defaults to 30 minutes — gives the user a meaningful
/// window of "I came back to my tab after a meeting" without the
/// previous turn paying the spawn cost again. Set very low in tests
/// so they don't have to wait.
pub async fn register_claude_provider(registry: &ProviderRegistry) {
    let provider = Arc::new(ClaudeProvider::new());
    let models = super::discover_models();

    provider.spawn_idle_reaper(30 * 60 * 1_000, Duration::from_secs(60));

    registry
        .register(
            provider,
            ProviderInfo {
                id: "claude".into(),
                display_name: "Claude (CLI)".into(),
                models,
                effort_levels: crate::provider::registry::standard_effort_levels(),
                capabilities: claude_capabilities(),
            },
        )
        .await;
}

/// What the Claude CLI provider actually supports — the long-lived
/// stream-json child gives it the richest capability set: in-band (soft)
/// interrupt with a hard-kill fallback, stdin answer delivery, and
/// mid-stream user-envelope injection.
pub fn claude_capabilities() -> crate::provider::registry::ProviderCapabilities {
    use crate::provider::registry::{AnswerTransport, InterruptKind, ProviderCapabilities};
    ProviderCapabilities {
        supports_thinking: true,
        supports_images_in: true,
        supports_usage: true,
        supports_resume: true,
        interrupt_kind: InterruptKind::Soft,
        supports_mid_stream_injection: true,
        answer_transport: AnswerTransport::Stdin,
    }
}
