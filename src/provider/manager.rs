use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::sync::OwnedMutexGuard;
use tokio::sync::mpsc;

use crate::db::Db;
use crate::db::models::{NewQueuedMessage, QueuedMessage};
use crate::plugin::manager::PluginManager;
use crate::provider::agent::{ProcessCompletion, SendMessageContext};
use crate::provider::message::UserMessage;
use crate::provider::registry::ProviderRegistry;
use crate::provider::stream::SpawnConfig;
use crate::ws::broadcaster::{Broadcaster, WsEvent};

/// Default provider id used when a model string has no `provider:` prefix.
pub const DEFAULT_PROVIDER: &str = "claude";

/// Outcome of `SessionManager::send_or_queue`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// The message was dispatched to the provider; an agent run started.
    Started,
    /// The session was already running an agent; the message was written to
    /// the persistent `queued_messages` queue and will be delivered when
    /// the current run completes.
    Queued,
}

/// What `send_or_queue` does with a message that arrives while the
/// session is already mid-turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidTurnPolicy {
    /// Persist in `queued_messages` and deliver when the current run
    /// completes. The default everywhere a human or another agent sends
    /// into a busy session: the working agent is never interrupted.
    Queue,
    /// Hand the message to the live turn when the provider supports
    /// mid-stream injection (the Claude CLI consumes stdin envelopes
    /// mid-run, which steers/interrupts the turn). Falls back to the
    /// queue when the provider can't inject. Reserved for flows that
    /// must reach the running agent now: ask_user answers and the
    /// explicit per-message "send now" force path.
    Inject,
}

/// Proof token: the bearer holds the per-session lock for `session_id`.
///
/// The only way to construct one is via `SessionManager::lock_session` or
/// `try_lock_session`, so a `&SessionLock` parameter on
/// `send_message_locked` is a compile-time guarantee that the caller has
/// serialised against every other `is_running → dispatch` decision for
/// this session. Pre-merge, four code paths were dispatching without the
/// lock and double-spawning agents; this type makes that bug a type error.
pub struct SessionLock {
    _guard: OwnedMutexGuard<()>,
    session_id: String,
}

impl SessionLock {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

/// Provider-agnostic dispatcher that owns the registry and routes session
/// operations to the right `AgentProvider` based on the model id.
///
/// Holds a per-session lock used by `send_or_queue` and `drain_queued`
/// so that the "is running? → spawn or enqueue" decision is atomic, and
/// the watchdog can detect in-flight handler work via `try_lock_session`.
pub struct SessionManager {
    registry: Arc<ProviderRegistry>,
    completion_tx: mpsc::Sender<ProcessCompletion>,
    completion_rx: Arc<Mutex<Option<mpsc::Receiver<ProcessCompletion>>>>,
    session_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// Plugin host handed to each provider via `SendMessageContext` so a
    /// non-Claude provider can drive todo lifecycle tracking through a plugin.
    /// Defaults to an empty (no-op) manager; the real one is wired in main via
    /// `with_plugins`.
    plugins: Arc<PluginManager>,
    /// Sudo-askpass wiring (helper script path, endpoint URL, token
    /// registry). `None` (tests, or the helper failed to write) means
    /// sessions get no `SUDO_ASKPASS` env and `sudo -A` stays unavailable.
    askpass: Option<crate::service::askpass::AskpassEnv>,
    /// Encrypted-env-var unlock registry — the SAME `Arc` as
    /// `AppState.env_unlock`. `None` in tests (custom env injection skipped).
    /// Wired from `main` via [`Self::with_env_unlock`].
    env_unlock: Option<Arc<crate::service::env_vars::EnvUnlockRegistry>>,
}

impl SessionManager {
    pub fn new(registry: Arc<ProviderRegistry>) -> Self {
        let (completion_tx, completion_rx) = mpsc::channel(64);
        SessionManager {
            registry,
            completion_tx,
            completion_rx: Arc::new(Mutex::new(Some(completion_rx))),
            session_locks: Arc::new(Mutex::new(HashMap::new())),
            plugins: Arc::new(PluginManager::empty()),
            askpass: None,
            env_unlock: None,
        }
    }

    /// Wire the sudo-askpass bridge so dispatched sessions can run
    /// `sudo -A` (masked password dialog in the UI, see `service::askpass`).
    pub fn with_askpass(mut self, askpass: Option<crate::service::askpass::AskpassEnv>) -> Self {
        self.askpass = askpass;
        self
    }

    /// Wire the encrypted-env-var unlock registry (same `Arc` as
    /// `AppState.env_unlock`) so dispatched interactive sessions can merge
    /// custom env vars and prompt to unlock encrypted ones over WS.
    pub fn with_env_unlock(
        mut self,
        registry: Option<Arc<crate::service::env_vars::EnvUnlockRegistry>>,
    ) -> Self {
        self.env_unlock = registry;
        self
    }

    /// The askpass registry wired via [`Self::with_askpass`], if any — the
    /// `/api/askpass` routes resolve tokens and pending requests through it.
    pub fn askpass_registry(&self) -> Option<&crate::service::askpass::AskpassRegistry> {
        self.askpass.as_ref().map(|a| &a.registry)
    }

    /// Attach the application's plugin host so providers dispatched by this
    /// manager can run their output through `todo`-hook plugins. Without this,
    /// `SendMessageContext::plugins` is an empty manager and plugin todo
    /// dispatch is a no-op.
    pub fn with_plugins(mut self, plugins: Arc<PluginManager>) -> Self {
        self.plugins = plugins;
        self
    }

    /// Take the completion receiver. Called once at startup to set up the
    /// worker-done listener loop.
    pub async fn take_completion_rx(&self) -> Option<mpsc::Receiver<ProcessCompletion>> {
        self.completion_rx.lock().await.take()
    }

    /// Acquire the per-session lock. All paths that mutate a session's run
    /// state (`send_or_queue`, `drain_queued`, the orchestrator spawn loop)
    /// MUST hold this lock to keep the "is_running → spawn or enqueue"
    /// decision atomic. The watchdog uses `try_lock_session` to skip
    /// sessions whose handler is mid-flight.
    ///
    /// The returned `SessionLock` is the proof token required to call
    /// `send_message_locked`.
    pub async fn lock_session(&self, session_id: &str) -> SessionLock {
        let lock = {
            let mut map = self.session_locks.lock().await;
            map.entry(session_id.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        SessionLock {
            _guard: lock.lock_owned().await,
            session_id: session_id.to_string(),
        }
    }

    /// Best-effort try-lock used by the watchdog. Returns None if the lock
    /// is currently held (i.e. a handler is mid-flight on this session).
    pub async fn try_lock_session(&self, session_id: &str) -> Option<SessionLock> {
        let lock = {
            let mut map = self.session_locks.lock().await;
            map.entry(session_id.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        lock.try_lock_owned().ok().map(|g| SessionLock {
            _guard: g,
            session_id: session_id.to_string(),
        })
    }

    /// Drop lock entries that nobody else holds a reference to.
    ///
    /// `lock_session` / `try_lock_session` clone the `Arc<Mutex<()>>`
    /// into the returned `SessionLock`'s guard, so a strong count > 1
    /// means a guard is live (or about to be live — see the race
    /// note below). A strong count of exactly 1 means only the map
    /// holds the `Arc` and the entry is provably unused; we can drop
    /// it and the next `lock_session` will re-create it transparently.
    ///
    /// The race: a caller about to call `lock_session` is racing
    /// against this sweep. Both paths take `self.session_locks.lock()`
    /// first, so we serialise on the outer map mutex. If the sweep
    /// wins, the caller just inserts a fresh entry on its next access
    /// — that's a no-op visible to callers since the `Arc<Mutex<()>>`
    /// for a session id has no persistent state.
    ///
    /// Returns the number of entries removed (for tests + tracing).
    /// Cost is O(N) over the map; intended for a low-frequency
    /// background sweep, not the hot path.
    pub async fn evict_idle_locks(&self) -> usize {
        let mut map = self.session_locks.lock().await;
        let before = map.len();
        map.retain(|_, lock| Arc::strong_count(lock) > 1);
        before - map.len()
    }

    /// Spawn a background task that periodically evicts idle lock-map
    /// entries. Returns the join handle so the caller (`main`) can hold
    /// it without leaking. Clones only the inner `Arc<Mutex<HashMap>>`
    /// — keeping the manager itself ownable-by-value lets `AppState`
    /// stay non-Arc, which matters because the hot paths (route
    /// handlers, orchestrator) already access it through `&AppState`.
    ///
    /// The sweep cadence is generous because the per-entry overhead is
    /// tiny (a HashMap bucket + an `Arc`) and we don't want this
    /// competing for the outer map mutex with hot-path dispatchers.
    pub fn spawn_lock_sweeper(&self) -> tokio::task::JoinHandle<()> {
        let locks = self.session_locks.clone();
        tokio::spawn(async move {
            const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);
            loop {
                tokio::time::sleep(SWEEP_INTERVAL).await;
                let mut map = locks.lock().await;
                let before = map.len();
                map.retain(|_, lock| Arc::strong_count(lock) > 1);
                let evicted = before - map.len();
                drop(map);
                if evicted > 0 {
                    tracing::debug!("Session lock sweep: evicted {evicted} idle entries");
                }
            }
        })
    }

    /// Dispatch a new agent run for `lock.session_id()`.
    ///
    /// The `&SessionLock` parameter is the compile-time proof that the
    /// per-session lock is held — every dispatch site must obtain one via
    /// `lock_session` (or `try_lock_session`) first, which serialises this
    /// call against every other `is_running → dispatch` decision for the
    /// same session. External callers should prefer the higher-level
    /// `send_or_queue` / `drain_queued`, which acquire the lock for you;
    /// reach for this directly only when you've already locked because
    /// you needed a custom check (e.g. the route handler that appends a
    /// user event before dispatching).
    pub async fn send_message_locked(
        &self,
        lock: &SessionLock,
        message: UserMessage,
        db: &Db,
        broadcaster: &Arc<Broadcaster>,
        config: SpawnConfig,
    ) -> anyhow::Result<()> {
        let session_id = lock.session_id();
        let session = db
            .get_session(session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("session not found: {}", session_id))?;

        // If a finalized handover/compaction left a doc waiting, prepend it
        // If a finalized handover/compaction left a doc waiting, or a review
        // switch / doc-review pass armed an injection, prepend it so the turn
        // opens with that context. Done here — the single dispatch chokepoint
        // — so the HTTP route, the queue drain, and the worker/repeating
        // paths all inject consistently.
        //
        // PEEK, not take: everything below here can still fail (folder
        // lookup, provider resolution, account env injection, spawn), and the
        // handover doc is the only surviving copy of the pre-reset
        // conversation. The columns are cleared at the very bottom, only once
        // `provider.send_message` returned Ok — so a failed dispatch leaves
        // them parked for the retry.
        //
        // The guard is a pure read of the already-loaded session row: skip
        // the extra DB round-trip when nothing at all is armed.
        let mut pending_used = crate::handover::PendingUsed::default();
        let message = if session.pending_handover_doc.is_some()
            || session.pending_plan_review
            || session.pending_doc_review.is_some()
        {
            let (text, used) =
                crate::handover::peek_pending_injection(db, session_id, &message.text).await;
            pending_used = used;
            UserMessage {
                text,
                attachments: message.attachments,
                attachment_ids: message.attachment_ids,
            }
        } else {
            message
        };
        let folder = db
            .get_folder(&session.folder_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("folder not found: {}", session.folder_id))?;

        // A blank `working_dir` means "caller didn't care" — but for a
        // worker on a worktree-isolated card that must still be the card's
        // worktree, not the shared checkout. Every resume path that builds a
        // SpawnConfig without the orchestrator (the chat route, question
        // answers, keepalive, handover) passes blank, and dispatching those
        // into the shared folder would drop edits outside the branch that
        // gets merged back.
        let working_dir = if config.working_dir.is_empty() {
            card_worktree_or_folder(db, &session, &folder.path).await
        } else {
            resolve_working_dir(&config.working_dir, &folder.path)
        };

        let conversation_id = if session.conversation_id.is_some() {
            session.conversation_id.clone()
        } else {
            self.find_conversation_id_from_events(db, session_id).await
        };

        // Session-level model overrides the request, matching the
        // pre-refactor behaviour.
        let requested_model = session
            .model
            .clone()
            .unwrap_or_else(|| config.model.clone());

        // Resolve effort once — it rides the spawn config AND drives the
        // auto-model routing below.
        let resolved_effort = session.effort.clone().or_else(|| config.effort.clone());

        // Unset model (legacy "default"/"auto" included): the app-wide
        // default-model setting wins; with none configured, fall back to
        // effort-based routing so a fresh install still dispatches sensibly.
        let final_model = if crate::provider::is_auto_model(&requested_model) {
            let db2 = db.clone();
            let configured = tokio::task::spawn_blocking(move || {
                db2.plugin_store_get_blocking(
                    crate::routes::settings::SETTINGS_NS,
                    crate::routes::settings::SETTINGS_COLLECTION,
                    crate::routes::settings::DEFAULT_MODEL_KEY,
                )
            })
            .await
            .ok()
            .and_then(|r| r.ok())
            .flatten()
            .and_then(|raw| {
                serde_json::from_str::<serde_json::Value>(&raw)
                    .ok()
                    .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(str::to_string))
            })
            .filter(|m| !crate::provider::is_auto_model(m));
            match configured {
                Some(m) => m,
                None => {
                    let candidates = self.registry.auto_model_candidates().await;
                    crate::provider::resolve_auto_model(
                        &candidates,
                        resolved_effort.as_deref(),
                        session.is_worker,
                    )?
                }
            }
        } else {
            requested_model
        };

        let (provider_id, _model_id) =
            ProviderRegistry::parse_model_id(&final_model, DEFAULT_PROVIDER);

        let provider = self
            .registry
            .get_provider(&provider_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("unknown agent provider: {}", provider_id))?;

        // Interactive caveman mode: the global `caveman_mode` app setting
        // appends a terse output-style block to every non-worker session's
        // system prompt (workers carry their own copy in the worker prompt).
        let system_prompt_suffix = if session.is_worker {
            config.system_prompt_suffix
        } else {
            let db2 = db.clone();
            let level = tokio::task::spawn_blocking(move || {
                db2.plugin_store_get_blocking(
                    crate::routes::settings::SETTINGS_NS,
                    crate::routes::settings::SETTINGS_COLLECTION,
                    "caveman_mode",
                )
            })
            .await
            .ok()
            .and_then(|r| r.ok())
            .flatten()
            .and_then(|raw| {
                serde_json::from_str::<serde_json::Value>(&raw)
                    .ok()
                    .and_then(|v| v.get("level").and_then(|l| l.as_str()).map(str::to_string))
            })
            .unwrap_or_default();
            match crate::provider::caveman_style(&level) {
                Some(style) => Some(match config.system_prompt_suffix {
                    Some(s) => format!("{s}\n{style}"),
                    None => style.to_string(),
                }),
                None => config.system_prompt_suffix,
            }
        };
        let mut final_config = SpawnConfig {
            working_dir,
            model: final_model,
            effort: resolved_effort,
            mcp_config_path: config.mcp_config_path,
            env: config.env,
            permission_mode: config.permission_mode,
            timeout_ms: config.timeout_ms,
            metadata: config.metadata,
            system_prompt_suffix,
            // A session's custom prompt is read here, once, so every dispatch
            // path (chat, worker, repeating task) honours it without each
            // caller having to thread it through SpawnConfig.
            system_prompt_override: session.system_prompt.clone(),
            // Resolve active-plugin tool names once here — the single dispatch
            // chokepoint — so the Claude provider can pre-approve the
            // common-tools file tools it now routes file access through.
            extra_allowed_tools: self
                .plugins
                .mcp_tools()
                .await
                .into_iter()
                .map(|t| t.name)
                .collect(),
            // Filled below, after this literal — the user's per-tool MCP
            // switches need the resolved provider id.
            extra_disallowed_tools: Vec::new(),
            // The authoritative worker flag — every dispatch path funnels
            // through here, so providers can trust it over whatever the
            // construction sites filled in.
            is_worker: session.is_worker,
            // Hard tool gate for pre-hatcher research sessions (see the
            // field docs on SpawnConfig).
            is_pre_hatcher: session.expert_kind.as_deref()
                == Some(crate::service::mcp_server::PRE_HATCHER_EXPERT_KIND),
        };
        // Permission-mode resolution, at the same dispatch chokepoint:
        // construction sites leave `permission_mode` unset (host default =
        // enforced — the Claude provider answers `can_use_tool` control
        // requests through its sandbox gate). The app-level escape hatch
        // (Settings → Claude Permissions) restores the legacy
        // `--dangerously-skip-permissions` behavior host-wide. Explicitly
        // set modes pass through untouched.
        if final_config.permission_mode.is_none()
            && crate::routes::settings::claude_bypass_permissions_for_db(db.clone()).await
        {
            final_config.permission_mode = Some("bypass".into());
        }
        // User-defined MCP servers (Settings → MCP Servers) merge into the
        // per-session config file here — the one spot every dispatch path
        // crosses AFTER the model (hence provider) is resolved; the
        // construction sites often only know `model: "default"`. Pre-hatcher
        // research sessions stay locked to the built-in read-only toolset.
        if !final_config.is_pre_hatcher {
            if let Some(path) = &final_config.mcp_config_path {
                crate::service::mcp_server::user_servers::append_user_mcp_servers(
                    path,
                    db,
                    &provider_id,
                )
                .await;
            }
            // Per-tool switches ride the same dispatch point: tools the user
            // disabled on applicable servers become hard-denied names for the
            // providers that can enforce them (Claude, Ollama).
            final_config.extra_disallowed_tools =
                crate::service::mcp_server::user_servers::disallowed_tool_names(
                    &crate::service::mcp_server::user_servers::load(db).await,
                    &provider_id,
                );
        }

        // Custom environment variables (Settings → Environment Variables) are
        // deliberately NOT injected into the agent process: an agent must not
        // be able to pull secret values out of its own environment. Commands
        // the agent runs receive them instead (see `plugin::host::exec_impl`,
        // which also masks secret values out of console output). Dispatch
        // still warms the encrypted-var unlock cache — the one moment an
        // interactive user is present to type the password.
        self.warm_env_unlock_cache(
            &final_config,
            db,
            broadcaster,
            session_id,
            &session.folder_id,
            session.repeating_task_id.is_some(),
        )
        .await;

        // Sudo askpass: interactive (non-worker) sessions get the helper +
        // a per-session secret so `sudo -A` inside the CLI child raises a
        // masked password dialog in the UI. Workers are headless (nobody is
        // watching a tab to type the password) and pre-hatchers are locked
        // read-only — both excluded.
        if !final_config.is_worker
            && !final_config.is_pre_hatcher
            && let Some(ap) = &self.askpass
        {
            let token = ap.registry.issue_token(session_id).await;
            final_config
                .env
                .insert("SUDO_ASKPASS".into(), ap.script_path.clone());
            final_config
                .env
                .insert("PECKBOARD_ASKPASS_URL".into(), ap.url.clone());
            final_config
                .env
                .insert("PECKBOARD_ASKPASS_TOKEN".into(), token);
        }

        let ctx = SendMessageContext {
            session_id: session_id.to_string(),
            message,
            db: db.clone(),
            broadcaster: broadcaster.clone(),
            config: final_config,
            conversation_id,
            completion_tx: self.completion_tx.clone(),
            // Stamp for this dispatch. The provider copies it into the
            // `ProcessCompletion` it emits, which is how the completion
            // listener tells a handover's doc turn from a completion left
            // over from an older run (see `handover::begin_handover`).
            run_id: crate::provider::agent::next_run_id(),
            plugins: self.plugins.clone(),
        };

        let result = provider.send_message(ctx).await;

        // The turn is away: only now is it safe to drop the one-shot
        // injection columns. Every `?` above returns without clearing, so a
        // dispatch that failed (missing folder, unknown provider, deleted
        // account, spawn error) leaves the handover doc, the plan review and
        // the doc review parked for the user's retry.
        if result.is_ok() {
            crate::handover::clear_pending_injections(db, session_id, &pending_used).await;
        }
        result
    }

    /// Warm the encrypted env var unlock cache at the dispatch chokepoint.
    ///
    /// Custom env vars (Settings → Environment Variables) are NOT merged into
    /// the agent's environment — agents must not be able to read secret
    /// values; commands they run receive the vars instead (see
    /// `plugin::host::exec_impl`). Dispatch only ensures encrypted values are
    /// unlockable by the time a command needs them: a cache hit needs
    /// nothing, a miss on an interactive session prompts the owner over WS
    /// (the unlock route decrypts and fills the cache), and a miss on a
    /// headless (worker / repeating / pre-hatcher) session is skipped
    /// silently. Never fails or delays the spawn: any db error logs a warn
    /// and skips. Decrypted values are never logged, broadcast, or persisted.
    /// Only vars visible to the session's folder (global + its own) are
    /// considered — no prompting for other folders' secrets.
    async fn warm_env_unlock_cache(
        &self,
        final_config: &SpawnConfig,
        db: &Db,
        broadcaster: &Arc<Broadcaster>,
        session_id: &str,
        folder_id: &str,
        is_repeating: bool,
    ) {
        let Some(registry) = &self.env_unlock else {
            return;
        };
        let rows = match db.list_env_vars().await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("env vars: list failed ({e}) — skipping unlock warm-up");
                return;
            }
        };

        // Encrypted vars, grouped by the owner who can unlock them.
        let mut owners: HashMap<String, Vec<String>> = HashMap::new();
        for row in &rows {
            let visible = row.folder_id.is_none() || row.folder_id.as_deref() == Some(folder_id);
            if row.encrypted
                && visible
                && let Some(owner) = &row.encrypted_by
            {
                owners
                    .entry(owner.clone())
                    .or_default()
                    .push(row.name.clone());
            }
        }
        if owners.is_empty() {
            return;
        }

        let interactive = !final_config.is_worker && !final_config.is_pre_hatcher && !is_repeating;
        for (owner, var_names) in owners {
            // Cache hit: the owner unlocked recently — commands can already
            // draw the values from the cache; nothing to prompt.
            if registry.cache_get(&owner).await.is_some() {
                continue;
            }
            // Headless sessions never prompt and never block.
            if !interactive {
                continue;
            }

            let (request_id, rx) = registry.begin_request(&owner, var_names.clone()).await;
            let username = db
                .get_user(&owner)
                .await
                .ok()
                .flatten()
                .map(|u| u.username)
                .unwrap_or_default();
            broadcaster.broadcast(WsEvent {
                event_type: "env-unlock-request".into(),
                session_id: session_id.to_string(),
                data: serde_json::json!({
                    "request_id": request_id,
                    "session_id": session_id,
                    "user_id": owner,
                    "username": username,
                    "var_names": var_names,
                }),
            });

            let reason = match tokio::time::timeout(
                std::time::Duration::from_secs(
                    crate::service::env_vars::UNLOCK_ANSWER_TIMEOUT_SECS,
                ),
                rx,
            )
            .await
            {
                // The unlock route decrypts and caches the values before
                // resolving; the payload here only signals the outcome.
                Ok(Ok(Some(_))) => "answered",
                Ok(Ok(None)) => "cancelled",
                Ok(Err(_)) => "dropped",
                Err(_) => {
                    registry.drop_request(&request_id).await;
                    "timeout"
                }
            };
            // Always resolve so stale dialogs on other tabs close (mirror
            // routes/askpass.rs).
            broadcaster.broadcast(WsEvent {
                event_type: "env-unlock-resolved".into(),
                session_id: session_id.to_string(),
                data: serde_json::json!({
                    "request_id": request_id,
                    "reason": reason,
                }),
            });
        }
    }

    /// Atomic check-and-act for the message dispatch path.
    ///
    /// Idle session: dispatches through `send_message_locked` (spawn or
    /// resume). Busy session: behaviour is decided by `policy` —
    ///
    /// - [`MidTurnPolicy::Queue`] (the default for user sends, agent-to-
    ///   agent messages, repeating tasks): persist the message in the
    ///   `queued_messages` FIFO and broadcast a queue event. The
    ///   completion listener calls `drain_queued` on agent-end, so the
    ///   running agent finishes its work untouched and queued messages
    ///   are delivered afterwards, oldest first, one turn each.
    ///
    /// - [`MidTurnPolicy::Inject`]: when the provider supports mid-stream
    ///   injection (Claude in stream-json mode) the user envelope is
    ///   written to the live child's stdin — the CLI folds it into the
    ///   running turn, which steers/interrupts the agent's current work.
    ///   Providers without that capability fall back to the queue.
    ///
    /// `user_event_appended`: true when the caller already appended the
    /// `user` event for this message (the /message route does, before
    /// calling here). Recorded on the queue row so the drain doesn't
    /// append a duplicate; when false the drain appends one at delivery.
    ///
    /// Callers MUST use this from any external trigger (HTTP route,
    /// orchestrator respawn). The per-session lock is held across
    /// the is_running check AND the dispatch so two concurrent
    /// sends never both decide to spawn.
    pub async fn send_or_queue(
        &self,
        session_id: &str,
        message: UserMessage,
        db: &Db,
        broadcaster: &Arc<Broadcaster>,
        config: SpawnConfig,
        policy: MidTurnPolicy,
        user_event_appended: bool,
    ) -> anyhow::Result<SendOutcome> {
        let lock = self.lock_session(session_id).await;
        let was_running = self.is_running(session_id).await;
        // Only meaningful when a run is live, and it MUST be answered by
        // the provider that owns that run — `config.model` can still be
        // unresolved ("default"), which parses to the default provider
        // regardless of what the session actually runs on.
        let supports_mid_stream = was_running
            && self
                .supports_mid_stream_for_session(session_id, &config.model)
                .await;

        if was_running && (policy == MidTurnPolicy::Queue || !supports_mid_stream) {
            // the current run finishes. Attachment ids ride along so a
            // queued send keeps its images — bytes are re-resolved from
            // the attachments dir at delivery time.
            let now = chrono::Utc::now().to_rfc3339();
            let attachment_ids = if message.attachment_ids.is_empty() {
                None
            } else {
                serde_json::to_string(&message.attachment_ids).ok()
            };
            let queued = db
                .enqueue_message(NewQueuedMessage {
                    session_id: session_id.to_string(),
                    text: message.text.clone(),
                    queued_at: now,
                    model: Some(config.model.clone()),
                    effort: config.effort.clone(),
                    attachment_ids,
                    user_event_appended,
                })
                .await?;
            broadcaster.broadcast(WsEvent {
                event_type: "queue".into(),
                session_id: session_id.to_string(),
                // No `text`: subscribers refetch `/api/sessions/:id/queue`
                // (which re-checks access) rather than trusting the frame.
                data: serde_json::json!({
                    "action": "set",
                    "id": queued.id,
                }),
            });
            tracing::info!(
                session_id = %session_id,
                queued_id = queued.id,
                "Agent mid-turn; message persisted in queue until it finishes"
            );
            return Ok(SendOutcome::Queued);
        }

        self.send_message_locked(&lock, message, db, broadcaster, config)
            .await?;

        if was_running {
            // Mid-turn inject: tell the session's subscribers the queue
            // changed. No text — they refetch the durable list.
            broadcaster.broadcast(WsEvent {
                event_type: "queue".into(),
                session_id: session_id.to_string(),
                data: serde_json::json!({ "action": "set" }),
            });
            tracing::info!(
                session_id = %session_id,
                "Mid-turn message delivered to provider stdin"
            );
            Ok(SendOutcome::Queued)
        } else {
            Ok(SendOutcome::Started)
        }
    }

    /// Whether a mid-turn message for `session_id` can be injected into
    /// the live turn.
    ///
    /// Asks the provider that actually owns the run — callers may pass an
    /// unresolved model (`"default"`/`"auto"`, as the ask_user answer path
    /// does), which `parse_model_id` maps to [`DEFAULT_PROVIDER`] and would
    /// wrongly report Claude's mid-stream support for an Ollama/Grok/
    /// Cursor/Mock session — dispatching a second concurrent run. Falls
    /// back to the model string only when no run is tracked, where the
    /// answer doesn't gate anything.
    async fn supports_mid_stream_for_session(&self, session_id: &str, model: &str) -> bool {
        match self.running_provider(session_id).await {
            Some(p) => p.supports_mid_stream_injection(),
            None => self.provider_for_model_supports_mid_stream(model).await,
        }
    }

    async fn provider_for_model_supports_mid_stream(&self, model: &str) -> bool {
        let (provider_id, _) = ProviderRegistry::parse_model_id(model, DEFAULT_PROVIDER);
        match self.registry.get_provider(&provider_id).await {
            Some(p) => p.supports_mid_stream_injection(),
            None => false,
        }
    }

    /// Drain the next queued message (oldest first) for `session_id` and
    /// dispatch it as a fresh agent run. Idempotent: if the queue is
    /// empty or an agent is already running, it returns `Ok(false)`
    /// without side effects. One message per call — the next agent-end
    /// triggers the next drain, so a backlog delivers as separate turns
    /// in FIFO order.
    ///
    /// Holds the per-session lock so it can't race with `send_or_queue`,
    /// the orchestrator, or another completion handler.
    pub async fn drain_queued(
        &self,
        session_id: &str,
        db: &Db,
        broadcaster: &Arc<Broadcaster>,
        config: SpawnConfig,
        data_dir: &std::path::Path,
    ) -> anyhow::Result<bool> {
        let lock = self.lock_session(session_id).await;

        if self.is_running(session_id).await {
            return Ok(false);
        }

        let queued = match db.next_queued_message(session_id).await? {
            Some(q) => q,
            None => return Ok(false),
        };

        let _ = db.delete_queued_message_by_id(session_id, queued.id).await;

        tracing::info!(
            session_id = %session_id,
            queued_id = queued.id,
            "Draining queued message and spawning agent run"
        );
        self.deliver_queued_row(&lock, queued, db, broadcaster, config, data_dir)
            .await?;
        Ok(true)
    }

    /// Force one queued message through immediately — the per-message
    /// "send now" button. The row must already be removed from the queue
    /// by the caller (so a racing drain can't deliver it twice).
    ///
    /// Under the per-session lock:
    /// - agent running, provider supports mid-stream injection (Claude):
    ///   the message is written into the live turn, which steers/
    ///   interrupts the agent's current work;
    /// - agent running, per-turn provider: the run is cancelled and the
    ///   message dispatched as a fresh run once termination completes;
    /// - agent idle: plain dispatch.
    ///
    /// The lock is held across cancel + wait + dispatch, so the
    /// completion listener's drain (triggered by the cancel's synthetic
    /// agent-end) blocks until the forced run is registered and then
    /// no-ops — the forced message can't be overtaken by the queue head.
    pub async fn force_queued(
        &self,
        session_id: &str,
        queued: QueuedMessage,
        db: &Db,
        broadcaster: &Arc<Broadcaster>,
        config: SpawnConfig,
        data_dir: &std::path::Path,
    ) -> anyhow::Result<()> {
        let lock = self.lock_session(session_id).await;

        if self.is_running(session_id).await
            && !self
                .supports_mid_stream_for_session(session_id, &config.model)
                .await
        {
            tracing::info!(
                session_id = %session_id,
                queued_id = queued.id,
                "Force-send: cancelling current run to deliver queued message"
            );
            self.cancel_and_wait(session_id).await;
        }

        self.deliver_queued_row(&lock, queued, db, broadcaster, config, data_dir)
            .await
    }

    /// Shared delivery tail for `drain_queued` / `force_queued`: append
    /// the `user` event when the enqueuer didn't, announce the drain,
    /// rebuild attachments from their ids, and dispatch.
    async fn deliver_queued_row(
        &self,
        lock: &SessionLock,
        queued: QueuedMessage,
        db: &Db,
        broadcaster: &Arc<Broadcaster>,
        config: SpawnConfig,
        data_dir: &std::path::Path,
    ) -> anyhow::Result<()> {
        let session_id = lock.session_id().to_string();

        // The /message route appends the `user` event at enqueue time so
        // the transcript shows the message where the user typed it; rows
        // queued by machine paths (POST /queue, agent-to-agent sends)
        // haven't been recorded yet, so the drain writes the event at
        // delivery. Exactly one of the two happens per message.
        if !queued.user_event_appended {
            let user_data = serde_json::json!({ "text": queued.text });
            match db
                .append_event(&session_id, "user", user_data.clone())
                .await
            {
                Ok(ev) => {
                    broadcaster.broadcast(WsEvent {
                        event_type: "event".into(),
                        session_id: session_id.clone(),
                        data: serde_json::json!({
                            "id": ev.id,
                            "seq": ev.seq,
                            "ts": ev.ts,
                            "kind": ev.kind,
                            "data": user_data,
                        }),
                    });
                }
                Err(e) => {
                    tracing::error!(
                        session_id = %session_id,
                        "deliver_queued_row: failed to append user event: {e}"
                    );
                }
            }
        }

        broadcaster.broadcast(WsEvent {
            event_type: "queue".into(),
            session_id: session_id.clone(),
            data: serde_json::json!({ "action": "drained", "id": queued.id }),
        });

        let attachment_ids: Vec<String> = queued
            .attachment_ids
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok())
            .unwrap_or_default();
        let mut attachments = Vec::with_capacity(attachment_ids.len());
        for aid in &attachment_ids {
            match crate::routes::attachments::load_attachment_payload(data_dir, &session_id, aid)
                .await
            {
                Some(p) => attachments.push(crate::provider::message::UserAttachment {
                    filename: p.filename,
                    mime_type: p.mime_type,
                    data: p.data,
                }),
                None => tracing::warn!(
                    session_id = %session_id,
                    attachment_id = %aid,
                    "Skipping attachment that vanished while queued"
                ),
            }
        }

        self.send_message_locked(
            lock,
            UserMessage {
                text: queued.text,
                attachments,
                attachment_ids,
            },
            db,
            broadcaster,
            config,
        )
        .await
    }

    /// Cancel the run for `session_id` across every registered provider.
    /// Each provider's `cancel` is a no-op if it isn't running that session,
    /// so fan-out is cheap and avoids needing a session→provider map.
    pub async fn cancel(&self, session_id: &str) {
        cancel_via_registry(&self.registry, session_id).await
    }

    /// Cancel the run for `session_id` and block until the background
    /// streaming task has actually wound down (including emitting any
    /// synthetic `agent-end` event from the cancel path).
    ///
    /// Required for any caller that wipes persistent state immediately
    /// after cancelling — e.g. `/sessions/:id/clear`. Without the wait,
    /// the synthetic Crashed event from the dying process lands AFTER
    /// the wipe and persists a stale "Agent crashed (interrupted)" line.
    pub async fn cancel_and_wait(&self, session_id: &str) {
        for info in self.registry.list_providers().await {
            if let Some(p) = self.registry.get_provider(&info.id).await {
                p.cancel(session_id).await;
                p.wait_for_termination(session_id).await;
            }
        }
    }

    pub async fn interrupt(&self, session_id: &str) {
        for info in self.registry.list_providers().await {
            if let Some(p) = self.registry.get_provider(&info.id).await {
                p.interrupt(session_id).await;
            }
        }
    }

    pub async fn write_stdin(&self, session_id: &str, text: &str) -> bool {
        for info in self.registry.list_providers().await {
            if let Some(p) = self.registry.get_provider(&info.id).await {
                if p.write_stdin(session_id, text).await {
                    return true;
                }
            }
        }
        false
    }

    /// The provider that currently owns a run for `session_id`, if any.
    ///
    /// The single source of truth for "which provider is this session
    /// actually on right now" — model strings can't answer that (they may
    /// be unresolved, and the session may have been switched since).
    pub async fn running_provider(
        &self,
        session_id: &str,
    ) -> Option<Arc<dyn crate::provider::agent::AgentProvider>> {
        for info in self.registry.list_providers().await {
            if let Some(p) = self.registry.get_provider(&info.id).await {
                if p.is_running(session_id).await {
                    return Some(p);
                }
            }
        }
        None
    }

    pub async fn is_running(&self, session_id: &str) -> bool {
        self.running_provider(session_id).await.is_some()
    }

    pub async fn cleanup(&self) {
        for info in self.registry.list_providers().await {
            if let Some(p) = self.registry.get_provider(&info.id).await {
                p.cleanup().await;
            }
        }
    }

    pub async fn shutdown(&self) {
        for info in self.registry.list_providers().await {
            if let Some(p) = self.registry.get_provider(&info.id).await {
                p.shutdown().await;
            }
        }
    }

    /// Scan the event tail for a conversation_id in agent-start or agent-end
    /// events. Used as a fallback when `session.conversation_id` is empty.
    async fn find_conversation_id_from_events(&self, db: &Db, session_id: &str) -> Option<String> {
        let tail = db.events_tail(session_id, 50).await.ok()?;
        resume_conversation_id_from_tail(&tail)
    }
}
/// Default working directory for a dispatch whose caller left `working_dir`
/// blank: the card's existing worktree when the session belongs to a card on
/// a `worktree_isolation` project, otherwise the shared folder.
///
/// Never creates a worktree — that stays with the orchestrator's spawn path
/// (`ensure_worktree`). A missing directory (isolation turned on after the
/// card started, or the worktree already merged away) means the shared
/// folder is the right answer.
async fn card_worktree_or_folder(
    db: &Db,
    session: &crate::db::models::Session,
    folder_path: &str,
) -> String {
    let (Some(project_id), Some(card_id)) = (&session.project_id, &session.card_id) else {
        return folder_path.to_string();
    };
    let isolation = db
        .get_project(project_id)
        .await
        .ok()
        .flatten()
        .map(|p| p.worktree_isolation)
        .unwrap_or(false);
    if !isolation {
        return folder_path.to_string();
    }
    let id8 = crate::worker::worktree::card_id8(card_id);
    let wt = crate::worker::worktree::worktree_path(folder_path, &id8);
    if wt.is_dir() {
        wt.to_string_lossy().to_string()
    } else {
        folder_path.to_string()
    }
}

/// Resolves the working directory a dispatch should run in: an explicit,
/// contained `requested` dir (e.g. a card's worktree from `ensure_worktree`)
/// if valid, otherwise the session's shared folder.
///
/// Falls back to `folder_path` whenever `requested` is blank, doesn't exist,
/// or — even after canonicalization — doesn't live inside `folder_path`. A
/// spawn must never be pointed outside the project folder, even if some
/// future caller computes a bad path.
pub(crate) fn resolve_working_dir(requested: &str, folder_path: &str) -> String {
    if requested.is_empty() || requested == folder_path {
        return folder_path.to_string();
    }
    let (Ok(canon_requested), Ok(canon_folder)) = (
        std::fs::canonicalize(requested),
        std::fs::canonicalize(folder_path),
    ) else {
        tracing::warn!(
            requested,
            folder_path,
            "resolve_working_dir: requested dir doesn't exist; using folder"
        );
        return folder_path.to_string();
    };
    if canon_requested.starts_with(&canon_folder) {
        canon_requested.to_string_lossy().to_string()
    } else {
        tracing::warn!(
            requested,
            folder_path,
            "resolve_working_dir: requested dir escapes the project folder; using folder"
        );
        folder_path.to_string()
    }
}

/// Newest-first scan of an event tail for a resumable conversationId.
///
/// A `handover` event marks a conversation reset (model switch or
/// compaction) — anything older belongs to the pre-reset conversation and
/// must never be resumed, so the scan stops there. In particular, the
/// doc-generation turn that precedes the `handover` event carries the OLD
/// conversationId in its agent-start/agent-end events; resuming it would
/// silently restore the entire pre-compaction history.
pub(crate) fn resume_conversation_id_from_tail(
    events: &[crate::db::models::Event],
) -> Option<String> {
    for event in events.iter().rev() {
        if event.kind == "handover" {
            return None;
        }
        if event.kind == "agent-start" || event.kind == "agent-end" {
            if let Ok(data) = serde_json::from_str::<serde_json::Value>(&event.data) {
                if let Some(cid) = data.get("conversationId").and_then(|v| v.as_str()) {
                    if !cid.is_empty() {
                        return Some(cid.to_string());
                    }
                }
            }
        }
    }

    None
}

/// Cancel `session_id` on every registered provider, identical fan-out to
/// [`SessionManager::cancel`], but reachable from places that only carry an
/// [`Arc<ProviderRegistry>`] (e.g. the MCP tool handlers, which receive the
/// registry through `ToolCallContext` rather than the full manager). Cheap:
/// each provider's `cancel` is a no-op when it isn't running the session.
pub async fn cancel_via_registry(registry: &ProviderRegistry, session_id: &str) {
    for info in registry.list_providers().await {
        if let Some(p) = registry.get_provider(&info.id).await {
            p.cancel(session_id).await;
        }
    }
}

/// Drop any queued follow-up message for `session_id` and tell the UI.
///
/// Call this from the **hard-stop** paths (the `/cancel` and `/terminate`
/// routes and their MCP equivalents) BEFORE cancelling the run. The
/// completion listener drains the queue on every completion — including the
/// synthetic one a cancel produces — so a queued message would otherwise
/// immediately respawn a fresh run. For per-turn providers (Ollama) a "send
/// while busy" follow-up always lands in the queue, so without this a
/// Terminate looks like it did nothing: the model just keeps streaming the
/// queued turn.
///
/// `/interrupt` deliberately does NOT call this — it is the "release the
/// current turn so my queued follow-up runs" affordance, and draining the
/// queue afterwards is its intended behaviour.
///
/// This is deliberately NOT folded into `SessionManager::cancel/interrupt`
/// or `drain_queued`: an *involuntary* termination (a crash) must still
/// drain so the user's queued message isn't stranded — only an explicit
/// hard stop discards it.
pub async fn clear_queued_message(db: &Db, broadcaster: &Arc<Broadcaster>, session_id: &str) {
    match db.clear_queued_messages(session_id).await {
        Ok(0) => {}
        Ok(_) => {
            broadcaster.broadcast(WsEvent {
                event_type: "queue".into(),
                session_id: session_id.to_string(),
                data: serde_json::json!({ "action": "deleted" }),
            });
        }
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                "clear_queued_message: failed to drop queued messages on stop: {e}"
            );
        }
    }
}

/// Request a graceful shutdown of `session_id` on every registered provider.
/// Fan-out mirrors [`cancel_via_registry`] but routes through each
/// provider's `shutdown_after_turn` so the in-flight turn (including any
/// outstanding tool response) is allowed to finish before the run is torn
/// down. Reach for this from the MCP terminal-step handlers
/// (`finish_card`, `complete_step`, `wont_do_card`) where a hard cancel
/// would race the tool response and surface as a worker crash even
/// though the card transition itself succeeded.
pub async fn shutdown_after_turn_via_registry(registry: &ProviderRegistry, session_id: &str) {
    for info in registry.list_providers().await {
        if let Some(p) = registry.get_provider(&info.id).await {
            p.shutdown_after_turn(session_id).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::registry::ProviderRegistry;

    fn manager() -> SessionManager {
        SessionManager::new(Arc::new(ProviderRegistry::new()))
    }
    #[test]
    fn resolve_working_dir_blank_uses_folder() {
        assert_eq!(resolve_working_dir("", "/some/folder"), "/some/folder");
    }

    #[test]
    fn resolve_working_dir_keeps_subdir_inside_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let folder_dir = tmp.path().join("folder");
        let sub_dir = folder_dir
            .join(".peckboard")
            .join("worktrees")
            .join("abcd1234");
        std::fs::create_dir_all(&sub_dir).unwrap();

        let resolved = resolve_working_dir(sub_dir.to_str().unwrap(), folder_dir.to_str().unwrap());
        assert_eq!(
            std::fs::canonicalize(resolved).unwrap(),
            std::fs::canonicalize(&sub_dir).unwrap()
        );
    }

    #[test]
    fn resolve_working_dir_rejects_escape_outside_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let folder_dir = tmp.path().join("folder");
        let outside_dir = tmp.path().join("outside");
        std::fs::create_dir_all(&folder_dir).unwrap();
        std::fs::create_dir_all(&outside_dir).unwrap();

        let resolved =
            resolve_working_dir(outside_dir.to_str().unwrap(), folder_dir.to_str().unwrap());
        assert_eq!(resolved, folder_dir.to_str().unwrap());
    }

    #[test]
    fn resolve_working_dir_falls_back_when_requested_missing() {
        let resolved = resolve_working_dir("/definitely/does/not/exist", "/some/folder");
        assert_eq!(resolved, "/some/folder");
    }

    #[tokio::test]
    async fn evict_idle_locks_removes_unheld_entries() {
        let m = manager();
        // Materialise three lock entries by acquiring + immediately
        // dropping each — after the drop the entry's Arc is held
        // only by the inner map, which is the precondition for
        // eviction.
        for id in ["s1", "s2", "s3"] {
            drop(m.lock_session(id).await);
        }
        assert_eq!(m.session_locks.lock().await.len(), 3);

        let evicted = m.evict_idle_locks().await;
        assert_eq!(evicted, 3);
        assert_eq!(m.session_locks.lock().await.len(), 0);
    }

    #[tokio::test]
    async fn evict_idle_locks_preserves_held_entries() {
        let m = manager();
        let held = m.lock_session("hot").await;
        drop(m.lock_session("cold").await);
        assert_eq!(m.session_locks.lock().await.len(), 2);

        // The hot lock is still live (we own its guard), so its
        // Arc has strong_count > 1; the cold one's only reference
        // is the map. Sweep must drop the cold one and leave the
        // hot one untouched.
        let evicted = m.evict_idle_locks().await;
        assert_eq!(evicted, 1);
        let remaining = m.session_locks.lock().await;
        assert_eq!(remaining.len(), 1);
        assert!(remaining.contains_key("hot"));
        drop(held);
    }

    #[tokio::test]
    async fn evict_idle_locks_is_no_op_on_empty_map() {
        let m = manager();
        assert_eq!(m.evict_idle_locks().await, 0);
    }

    #[tokio::test]
    async fn lock_session_after_eviction_works() {
        // Regression check: a sweep that just dropped an entry must
        // not break a subsequent lock_session for the same id —
        // it should transparently re-create the entry.
        let m = manager();
        drop(m.lock_session("s1").await);
        m.evict_idle_locks().await;

        // Re-acquire — must succeed and yield a fresh, working lock.
        let lock = m.lock_session("s1").await;
        assert_eq!(lock.session_id(), "s1");
    }

    fn make_event(kind: &str, data: &str) -> crate::db::models::Event {
        crate::db::models::Event {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: "s1".into(),
            seq: 0,
            ts: 0,
            kind: kind.into(),
            data: data.into(),
        }
    }

    #[test]
    fn resume_scan_finds_latest_conversation_id() {
        let events = vec![
            make_event("agent-start", r#"{"conversationId":"conv-old"}"#),
            make_event("agent-end", r#"{"conversationId":"conv-new"}"#),
        ];
        assert_eq!(
            resume_conversation_id_from_tail(&events),
            Some("conv-new".into())
        );
    }

    #[test]
    fn resume_scan_stops_at_handover() {
        // Regression: after a compaction, the doc-generation turn's
        // agent-start/agent-end still carry the pre-reset conversationId.
        // The `handover` event that follows them must act as a barrier —
        // resuming past it restores the entire pre-compaction history.
        let events = vec![
            make_event("agent-start", r#"{"conversationId":"conv-old"}"#),
            make_event("agent-end", r#"{"conversationId":"conv-old"}"#),
            make_event("handover-start", r#"{"compaction":true}"#),
            make_event("agent-start", r#"{"conversationId":"conv-old"}"#),
            make_event("agent-end", r#"{"conversationId":"conv-old"}"#),
            make_event("handover", r#"{"compaction":true}"#),
        ];
        assert_eq!(resume_conversation_id_from_tail(&events), None);
    }

    #[test]
    fn resume_scan_finds_post_handover_conversation() {
        let events = vec![
            make_event("agent-end", r#"{"conversationId":"conv-old"}"#),
            make_event("handover", r#"{"compaction":true}"#),
            make_event("agent-start", r#"{"conversationId":"conv-fresh"}"#),
        ];
        assert_eq!(
            resume_conversation_id_from_tail(&events),
            Some("conv-fresh".into())
        );
    }

    /// Stand-in provider with a scriptable run state and mid-stream
    /// capability, so the dispatch decision can be tested without a real
    /// CLI behind it.
    struct StubProvider {
        id: &'static str,
        running: bool,
        mid_stream: bool,
    }

    #[async_trait::async_trait]
    impl crate::provider::agent::AgentProvider for StubProvider {
        fn id(&self) -> &str {
            self.id
        }
        async fn send_message(
            &self,
            _ctx: crate::provider::agent::SendMessageContext,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn cancel(&self, _session_id: &str) {}
        async fn interrupt(&self, _session_id: &str) {}
        async fn write_stdin(&self, _session_id: &str, _text: &str) -> bool {
            true
        }
        async fn is_running(&self, _session_id: &str) -> bool {
            self.running
        }
        fn supports_mid_stream_injection(&self) -> bool {
            self.mid_stream
        }
        async fn cleanup(&self) {}
        async fn shutdown(&self) {}
    }

    async fn manager_with(stubs: Vec<StubProvider>) -> SessionManager {
        let registry = Arc::new(ProviderRegistry::new());
        for stub in stubs {
            let info = crate::provider::registry::ProviderInfo {
                id: stub.id.to_string(),
                display_name: stub.id.to_string(),
                models: Vec::new(),
                effort_levels: Vec::new(),
                capabilities: Default::default(),
            };
            registry.register(Arc::new(stub), info).await;
        }
        SessionManager::new(registry)
    }

    fn claude_stub(running: bool) -> StubProvider {
        StubProvider {
            id: DEFAULT_PROVIDER,
            running,
            mid_stream: true,
        }
    }

    fn mock_stub(running: bool) -> StubProvider {
        StubProvider {
            id: "mock",
            running,
            mid_stream: false,
        }
    }

    #[tokio::test]
    async fn unresolved_model_uses_the_running_providers_capability() {
        // Regression: `"default"` has no provider prefix, so parsing it
        // yields `claude` — which DOES support mid-stream injection — even
        // though the live run belongs to the mock provider, which does not.
        let m = manager_with(vec![claude_stub(false), mock_stub(true)]).await;
        assert!(m.provider_for_model_supports_mid_stream("default").await);
        assert!(!m.supports_mid_stream_for_session("s1", "default").await);
    }

    #[tokio::test]
    async fn running_mid_stream_provider_still_injects() {
        let m = manager_with(vec![claude_stub(true), mock_stub(false)]).await;
        assert!(m.supports_mid_stream_for_session("s1", "default").await);
        // A stale model string from another provider doesn't demote the
        // live run either.
        assert!(m.supports_mid_stream_for_session("s1", "mock:echo").await);
    }

    #[tokio::test]
    async fn falls_back_to_the_model_string_when_nothing_runs() {
        let m = manager_with(vec![claude_stub(false), mock_stub(false)]).await;
        assert!(m.supports_mid_stream_for_session("s1", "default").await);
        assert!(!m.supports_mid_stream_for_session("s1", "mock:echo").await);
    }

    #[tokio::test]
    async fn inject_with_unresolved_model_queues_on_non_mid_stream_provider() {
        // The ask_user answer path hardcodes model `"default"` + Inject.
        // With the mock provider mid-turn, that answer MUST land in the
        // queue — dispatching it would spawn a second concurrent run on
        // the same session.
        let m = manager_with(vec![claude_stub(false), mock_stub(true)]).await;
        let db = crate::db::Db::in_memory().unwrap();
        let broadcaster = crate::ws::broadcaster::Broadcaster::new();
        let now = chrono::Utc::now().to_rfc3339();
        db.create_folder(crate::db::models::NewFolder {
            id: "f1".into(),
            name: "f1".into(),
            path: "/tmp/f1".into(),
            created_at: now.clone(),
        })
        .await
        .unwrap();
        db.create_session(crate::db::models::NewSession {
            id: "s1".into(),
            name: "s1".into(),
            folder_id: "f1".into(),
            created_at: now.clone(),
            last_activity: now,
            ..Default::default()
        })
        .await
        .unwrap();

        let outcome = m
            .send_or_queue(
                "s1",
                crate::provider::message::UserMessage::from_text("answer"),
                &db,
                &broadcaster,
                SpawnConfig {
                    model: "default".into(),
                    ..Default::default()
                },
                MidTurnPolicy::Inject,
                true,
            )
            .await
            .unwrap();

        assert!(matches!(outcome, SendOutcome::Queued));
        let queued = db.next_queued_message("s1").await.unwrap();
        assert_eq!(queued.map(|q| q.text).as_deref(), Some("answer"));
    }
}
