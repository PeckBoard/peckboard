use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::sync::OwnedMutexGuard;
use tokio::sync::mpsc;

use crate::db::Db;
use crate::db::models::NewQueuedMessage;
use crate::plugin::manager::PluginManager;
use crate::provider::agent::{ProcessCompletion, SendMessageContext};
use crate::provider::message::UserMessage;
use crate::provider::registry::ProviderRegistry;
use crate::provider::stream::SpawnConfig;
use crate::ws::broadcaster::{Broadcaster, WsEvent};

/// Default provider id used when a model string has no `provider:` prefix.
const DEFAULT_PROVIDER: &str = "claude";

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
        }
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

        let folder = db
            .get_folder(&session.folder_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("folder not found: {}", session.folder_id))?;

        let working_dir = folder.path.clone();

        let conversation_id = if session.conversation_id.is_some() {
            session.conversation_id.clone()
        } else {
            self.find_conversation_id_from_events(db, session_id).await
        };

        // Session-level model overrides the request, matching the
        // pre-refactor behaviour.
        let final_model = session
            .model
            .clone()
            .unwrap_or_else(|| config.model.clone());

        let (provider_id, _model_id) =
            ProviderRegistry::parse_model_id(&final_model, DEFAULT_PROVIDER);

        let provider = self
            .registry
            .get_provider(&provider_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("unknown agent provider: {}", provider_id))?;

        let final_config = SpawnConfig {
            working_dir,
            model: final_model,
            effort: session.effort.or(config.effort),
            mcp_config_path: config.mcp_config_path,
            env: config.env,
            permission_mode: config.permission_mode,
            timeout_ms: config.timeout_ms,
            metadata: config.metadata,
            system_prompt_suffix: config.system_prompt_suffix,
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
        };

        let ctx = SendMessageContext {
            session_id: session_id.to_string(),
            message,
            db: db.clone(),
            broadcaster: broadcaster.clone(),
            config: final_config,
            conversation_id,
            completion_tx: self.completion_tx.clone(),
            plugins: self.plugins.clone(),
        };

        provider.send_message(ctx).await
    }

    /// Atomic check-and-act for the message dispatch path.
    ///
    /// Behaviour forks on the underlying provider's
    /// `supports_mid_stream_injection` capability:
    ///
    /// - **Mid-stream-capable (Claude in stream-json mode).** Always
    ///   dispatches through `send_message_locked`. The provider
    ///   either spawns a fresh child (first turn / after idle reap)
    ///   or writes the new user envelope to the existing child's
    ///   stdin. There is no DB-level queue — the CLI itself is the
    ///   queue. `SendOutcome::Queued` is reported when a turn was
    ///   already in flight at dispatch time so the UI can render
    ///   the "will pick up after this turn" badge.
    ///
    /// - **Per-turn provider (mock + any future provider that can
    ///   only handle one turn at a time).** Falls back to the
    ///   original behaviour: if `is_running`, persist the message in
    ///   `queued_messages` and broadcast a queue event; the
    ///   completion listener calls `drain_queued` to deliver it
    ///   when the current run ends. Otherwise dispatch directly.
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
    ) -> anyhow::Result<SendOutcome> {
        let lock = self.lock_session(session_id).await;
        let was_running = self.is_running(session_id).await;
        let supports_mid_stream = self
            .provider_for_model_supports_mid_stream(&config.model)
            .await;

        if was_running && !supports_mid_stream {
            // Per-turn provider — fall back to the durable queue so
            // the completion listener can deliver this message when
            // the current run finishes. The persistent queue is
            // text-only; per-turn providers (mock, ollama) can't
            // make use of multimodal attachments, so dropping them
            // here is lossless for the providers that actually take
            // this branch.
            let now = chrono::Utc::now().to_rfc3339();
            db.upsert_queued_message(NewQueuedMessage {
                session_id: session_id.to_string(),
                text: message.text.clone(),
                queued_at: now,
                model: Some(config.model.clone()),
                effort: config.effort.clone(),
            })
            .await?;
            broadcaster.broadcast(WsEvent {
                event_type: "queue".into(),
                session_id: session_id.to_string(),
                data: serde_json::json!({ "action": "set", "text": message.text }),
            });
            tracing::info!(
                session_id = %session_id,
                "Per-turn provider already running; message persisted in queue"
            );
            return Ok(SendOutcome::Queued);
        }

        // Capture the text for the post-dispatch broadcast — the
        // dispatch path moves the whole `UserMessage` into the
        // provider context.
        let mid_turn_text = if was_running {
            Some(message.text.clone())
        } else {
            None
        };

        self.send_message_locked(&lock, message, db, broadcaster, config)
            .await?;

        if let Some(text) = mid_turn_text {
            broadcaster.broadcast(WsEvent {
                event_type: "queue".into(),
                session_id: session_id.to_string(),
                data: serde_json::json!({ "action": "set", "text": text }),
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

    async fn provider_for_model_supports_mid_stream(&self, model: &str) -> bool {
        let (provider_id, _) = ProviderRegistry::parse_model_id(model, DEFAULT_PROVIDER);
        match self.registry.get_provider(&provider_id).await {
            Some(p) => p.supports_mid_stream_injection(),
            None => false,
        }
    }

    /// Drain the persistent queued message (if any) for `session_id` and
    /// dispatch it as a fresh agent run. Idempotent: if there is no
    /// queued message or an agent is already running, it returns
    /// `Ok(false)` without side effects.
    ///
    /// Holds the per-session lock so it can't race with `send_or_queue`,
    /// the orchestrator, or another completion handler.
    pub async fn drain_queued(
        &self,
        session_id: &str,
        db: &Db,
        broadcaster: &Arc<Broadcaster>,
        config: SpawnConfig,
    ) -> anyhow::Result<bool> {
        let lock = self.lock_session(session_id).await;

        if self.is_running(session_id).await {
            return Ok(false);
        }

        let queued = match db.get_queued_message(session_id).await? {
            Some(q) => q,
            None => return Ok(false),
        };

        let _ = db.delete_queued_message(session_id).await;

        // Persist the queued text as a user event so the conversation log
        // reflects the actual delivery order (queue write → drain on
        // completion → user event → agent run).
        let user_data = serde_json::json!({ "text": queued.text });
        match db.append_event(session_id, "user", user_data.clone()).await {
            Ok(ev) => {
                broadcaster.broadcast(WsEvent {
                    event_type: "event".into(),
                    session_id: session_id.to_string(),
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
                    "drain_queued: failed to append user event: {e}"
                );
            }
        }

        broadcaster.broadcast(WsEvent {
            event_type: "queue".into(),
            session_id: session_id.to_string(),
            data: serde_json::json!({ "action": "drained" }),
        });

        tracing::info!(
            session_id = %session_id,
            "Draining queued message and spawning agent run"
        );
        self.send_message_locked(
            &lock,
            UserMessage::from_text(queued.text),
            db,
            broadcaster,
            config,
        )
        .await?;
        Ok(true)
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

    pub async fn is_running(&self, session_id: &str) -> bool {
        for info in self.registry.list_providers().await {
            if let Some(p) = self.registry.get_provider(&info.id).await {
                if p.is_running(session_id).await {
                    return true;
                }
            }
        }
        false
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

        for event in tail.iter().rev() {
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
    match db.delete_queued_message(session_id).await {
        Ok(true) => {
            broadcaster.broadcast(WsEvent {
                event_type: "queue".into(),
                session_id: session_id.to_string(),
                data: serde_json::json!({ "action": "deleted" }),
            });
        }
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                "clear_queued_message: failed to drop queued message on stop: {e}"
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
}
