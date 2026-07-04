//! Provider login keep-alive.
//!
//! Auth tokens for the CLI-backed providers (Claude, Grok, Cursor) go stale
//! if a login sits unused. This background task periodically spins up a
//! throwaway session per login, sends a one-word "hi", waits for the turn to
//! finish, then tears the session down — just enough real traffic to refresh
//! the token.
//!
//! Cost is kept near zero on purpose: every cycle uses a **fresh** session,
//! the session's system prompt is replaced with a single "." (skipping the
//! large Peckboard prompt), no MCP tools are wired in, and the user message
//! is a single token.
//!
//! Scope is "every auth login": for Claude and Grok that means the host
//! default login **and** each stored account (the account rides on the model
//! id as an `@<account_id>` suffix, which the provider turns into per-account
//! credential env). Cursor has a single system login. Mock/Ollama have no
//! login and are skipped.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use crate::db::Db;
use crate::db::models::{NewFolder, NewSession};
use crate::provider::manager::SessionManager;
use crate::provider::message::UserMessage;
use crate::provider::registry::ProviderRegistry;
use crate::provider::stream::SpawnConfig;
use crate::state::AppState;
use crate::ws::broadcaster::Broadcaster;

/// Fixed id of the hidden folder keep-alive sessions attach to. Filtered out
/// of `GET /api/folders` (see `routes::folders`) so it never shows in the UI.
pub const KEEPALIVE_FOLDER_ID: &str = "__peckboard_keepalive__";

/// Providers with a login worth refreshing, in dispatch order. Mock/Ollama
/// have no remote auth and are intentionally absent.
const AUTH_PROVIDERS: &[&str] = &["claude", "grok", "cursor"];

/// Providers whose logins are multi-account (a login per stored account plus
/// the host default). Others get a single default ping.
const MULTI_ACCOUNT_PROVIDERS: &[&str] = &["claude", "grok"];

/// Per-login cap: how long to wait for the "hi" turn before force-killing the
/// run and cleaning up. A hung/unauthenticated CLI is bounded by this.
const RUN_TIMEOUT: Duration = Duration::from_secs(90);

/// One recorded keep-alive: which login (provider + optional account) it
/// refreshed, a human label for the UI, and when it last ran (RFC3339).
#[derive(Clone, serde::Serialize)]
pub struct LastRun {
    pub provider: String,
    /// `None` for the host's own default login; the stored account id otherwise.
    pub account_id: Option<String>,
    pub label: String,
    pub at: String,
}

/// Per-login last-run times for the current process, keyed by
/// `provider:{account_id|default}`. Process-ephemeral (reset on restart) — a
/// diagnostic surfaced in Settings via `GET /api/config`, not durable state,
/// so it lives here instead of the DB (no migration).
static LAST_RUNS: LazyLock<Mutex<HashMap<String, LastRun>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Last-run record for every login pinged since startup, newest first.
pub fn last_runs() -> Vec<LastRun> {
    let mut runs: Vec<LastRun> = LAST_RUNS
        .lock()
        .map(|m| m.values().cloned().collect())
        .unwrap_or_default();
    runs.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| a.label.cmp(&b.label)));
    runs
}

/// Stamp `target` as refreshed at `at` (RFC3339).
fn record_run(target: &Target, at: String) {
    if let Ok(mut runs) = LAST_RUNS.lock() {
        runs.insert(
            target.key(),
            LastRun {
                provider: target.provider.clone(),
                account_id: target.account_id.clone(),
                label: target.label.clone(),
                at,
            },
        );
    }
}

/// Spawn the keep-alive loop. No-op when `interval_hours == 0` (disabled).
pub fn spawn(state: Arc<AppState>, interval_hours: u64) {
    if interval_hours == 0 {
        tracing::info!("Provider keep-alive disabled (interval 0)");
        return;
    }
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(interval_hours * 3600));
        // A slow cycle shouldn't burst catch-up runs; the next tick re-pings
        // everything anyway.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Skip the immediate first tick — don't ping the moment the server
        // boots (that races the rest of startup); first run is one interval in.
        interval.tick().await;
        loop {
            interval.tick().await;
            run_once(
                &state.db,
                &state.provider_registry,
                &state.session_manager,
                &state.broadcaster,
                &state.config.data_dir,
            )
            .await;
        }
    });
    tracing::info!("Provider keep-alive started ({interval_hours}h interval)");
}

/// A single login to keep alive: which provider/account it selects, a human
/// label for logs and the UI, and the fully qualified `provider:model[@account]`
/// id that selects it.
struct Target {
    provider: String,
    /// `None` for the host default login; the stored account id otherwise.
    account_id: Option<String>,
    label: String,
    model: String,
}

impl Target {
    /// Stable per-login key: `provider:{account_id|default}`.
    fn key(&self) -> String {
        format!(
            "{}:{}",
            self.provider,
            self.account_id.as_deref().unwrap_or("default")
        )
    }
}

/// Run one keep-alive cycle: ping every configured login once. Never returns
/// an error — a failure on one login is logged and the rest still run.
pub async fn run_once(
    db: &Db,
    registry: &ProviderRegistry,
    session_manager: &SessionManager,
    broadcaster: &Arc<Broadcaster>,
    data_dir: &Path,
) {
    let folder_id = match ensure_folder(db, data_dir).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!("keep-alive: could not prepare folder: {e}");
            return;
        }
    };

    let targets = collect_targets(db, registry).await;
    if targets.is_empty() {
        tracing::debug!("keep-alive: no auth logins to refresh");
        return;
    }

    for target in targets {
        // Stamp the login as run when its ping starts, so "last run" reflects
        // this cycle even if the ping itself then fails.
        let at = chrono::Utc::now().to_rfc3339();
        record_run(&target, at.clone());
        match ping(db, session_manager, broadcaster, &folder_id, &target).await {
            Ok(()) => tracing::info!(login = %target.label, at = %at, "keep-alive ping ok"),
            Err(e) => {
                tracing::warn!(login = %target.label, at = %at, "keep-alive ping failed: {e}")
            }
        }
    }
}

/// Build the list of logins to ping from the registered providers plus the
/// stored account tables.
async fn collect_targets(db: &Db, registry: &ProviderRegistry) -> Vec<Target> {
    let mut targets = Vec::new();

    for &provider in AUTH_PROVIDERS {
        // Static model list (un-suffixed base ids); skip a provider that
        // isn't registered or exposes no models (e.g. Cursor when unauthed).
        let Some(info) = registry.get_info(provider).await else {
            continue;
        };
        let Some(base) = info.models.first() else {
            continue;
        };
        let base_id = &base.id;

        // Host default login (no account suffix).
        targets.push(Target {
            provider: provider.to_string(),
            account_id: None,
            label: format!("{provider} (default)"),
            model: format!("{provider}:{base_id}"),
        });

        if !MULTI_ACCOUNT_PROVIDERS.contains(&provider) {
            continue;
        }

        // One extra login per stored account, keyed by the `@<id>` suffix
        // the provider resolves to per-account credentials.
        let accounts = match provider {
            "claude" => db
                .list_claude_accounts()
                .await
                .map(|v| v.into_iter().map(|a| (a.id, a.name)).collect::<Vec<_>>()),
            "grok" => db
                .list_grok_accounts()
                .await
                .map(|v| v.into_iter().map(|a| (a.id, a.name)).collect::<Vec<_>>()),
            _ => Ok(Vec::new()),
        };
        match accounts {
            Ok(accounts) => {
                for (id, name) in accounts {
                    targets.push(Target {
                        provider: provider.to_string(),
                        account_id: Some(id.clone()),
                        label: format!("{provider} ({name})"),
                        model: format!("{provider}:{base_id}@{id}"),
                    });
                }
            }
            Err(e) => tracing::warn!("keep-alive: failed to list {provider} accounts: {e}"),
        }
    }

    targets
}

/// Get-or-create the hidden folder keep-alive sessions attach to. Its path is
/// an empty dir under the data dir so the CLI has a valid, inert cwd.
async fn ensure_folder(db: &Db, data_dir: &Path) -> anyhow::Result<String> {
    if db.get_folder(KEEPALIVE_FOLDER_ID).await?.is_some() {
        return Ok(KEEPALIVE_FOLDER_ID.to_string());
    }
    let path = data_dir.join("keepalive");
    std::fs::create_dir_all(&path)?;
    let now = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: KEEPALIVE_FOLDER_ID.to_string(),
        name: "Keep-alive".to_string(),
        path: path.to_string_lossy().to_string(),
        created_at: now,
    })
    .await?;
    Ok(KEEPALIVE_FOLDER_ID.to_string())
}

/// Ping one login: create a throwaway worker session, send "hi", wait for the
/// turn to finish (bounded by [`RUN_TIMEOUT`]), then delete the session and
/// its events. Cleanup runs even if the dispatch itself errors.
async fn ping(
    db: &Db,
    session_manager: &SessionManager,
    broadcaster: &Arc<Broadcaster>,
    folder_id: &str,
    target: &Target,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    let session_id = uuid::Uuid::new_v4().to_string();

    db.create_session(NewSession {
        id: session_id.clone(),
        name: format!("keep-alive: {}", target.label),
        folder_id: folder_id.to_string(),
        model: Some(target.model.clone()),
        effort: None,
        // Worker-flagged so it's excluded from the sessions list for the
        // few seconds it exists; deleted directly (not via the HTTP route,
        // which refuses worker sessions).
        is_worker: true,
        // A non-empty system prompt FULLY replaces the standing Peckboard
        // prompt — a single "." keeps the turn's token count near zero.
        system_prompt: Some(".".to_string()),
        created_at: now.clone(),
        last_activity: now,
        ..Default::default()
    })
    .await?;

    // Dispatch a one-token message with no MCP tools wired in.
    let config = SpawnConfig {
        model: target.model.clone(),
        effort: None,
        working_dir: String::new(), // filled from the folder by the manager
        mcp_config_path: None,
        env: Default::default(),
        permission_mode: Some("bypass".into()),
        timeout_ms: Some(RUN_TIMEOUT.as_millis() as u64),
        metadata: serde_json::Value::Null,
        system_prompt_suffix: None,
        system_prompt_override: None,
        extra_allowed_tools: Vec::new(),
        // Set from the session row in SessionManager::final_config.
        is_worker: false,
    };

    let dispatch = session_manager
        .send_or_queue(
            &session_id,
            UserMessage::from_text("hi"),
            db,
            broadcaster,
            config,
        )
        .await;

    if dispatch.is_ok() {
        wait_until_idle(session_manager, &session_id, RUN_TIMEOUT).await;
    }

    // Always tear down: kill any lingering child, then remove the session and
    // its events (FKs are enforced, so events must go first).
    session_manager.cancel_and_wait(&session_id).await;
    let _ = db.delete_events_by_session(&session_id).await;
    let _ = db.delete_session(&session_id).await;

    dispatch.map(|_| ())
}

/// Poll until the session's run finishes or `timeout` elapses.
async fn wait_until_idle(session_manager: &SessionManager, session_id: &str, timeout: Duration) {
    let start = tokio::time::Instant::now();
    // Give the async spawn a moment to register as running before we start
    // checking, so we don't observe "idle" before the turn has even begun.
    tokio::time::sleep(Duration::from_millis(500)).await;
    while start.elapsed() < timeout {
        if !session_manager.is_running(session_id).await {
            return;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
