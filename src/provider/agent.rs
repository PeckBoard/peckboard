use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::db::Db;
use crate::plugin::manager::PluginManager;
use crate::provider::message::UserMessage;
use crate::provider::stream::{ModelInfo, ProviderEvent, SpawnConfig};
use crate::ws::broadcaster::{Broadcaster, WsEvent};

/// Notification sent when an agent run finishes streaming.
///
/// Delivered to the dispatcher's completion channel so the worker
/// orchestrator can react (e.g. advance a card) outside the streaming
/// task itself.
pub struct ProcessCompletion {
    pub session_id: String,
    pub completed: bool,
    /// Provider-reported error text when the run's final turn failed
    /// (e.g. "Failed to authenticate. API Error: 401 …" from an expired
    /// login). `None` for clean completions and for providers that don't
    /// surface per-turn errors. The completion listener threads it into
    /// `abort_handover` so a failed compaction/handover shows the user
    /// WHY it was rolled back.
    pub error: Option<String>,
}

/// Context handed to an `AgentProvider` when a new run is requested.
///
/// The dispatcher resolves the working directory, the (optional) resume
/// `conversation_id`, and the completion channel so individual providers
/// don't need to repeat that bookkeeping.
pub struct SendMessageContext {
    pub session_id: String,
    pub message: UserMessage,
    pub db: Db,
    pub broadcaster: Arc<Broadcaster>,
    pub config: SpawnConfig,
    pub conversation_id: Option<String>,
    pub completion_tx: mpsc::Sender<ProcessCompletion>,
    /// Plugin host for this dispatch. A non-Claude provider runs its raw
    /// output through `crate::plugin::todo_hook::emit_plugin_todos` to let a
    /// `todo`-hook plugin drive lifecycle tracking. Empty (a no-op) unless the
    /// dispatching `SessionManager` was built with `with_plugins`.
    pub plugins: Arc<PluginManager>,
}

/// An agent provider runs agent sessions of one kind (Claude CLI, a mock,
/// a future cloud-hosted agent, etc.) and translates their output into the
/// unified `ProviderEvent` stream that the rest of Peckboard consumes.
///
/// Providers are stored as `Arc<dyn AgentProvider>` in `ProviderRegistry`
/// and looked up by `id()` (the prefix in model strings like `claude:opus`).
///
/// All methods take `&self` — providers track per-session state internally
/// via interior mutability (e.g. `Arc<Mutex<HashMap<...>>>`).
#[async_trait]
pub trait AgentProvider: Send + Sync + 'static {
    /// Stable identifier (e.g. `"claude"`, `"mock"`). Used as the prefix in
    /// fully-qualified model IDs like `"claude:claude-opus-4-7"`.
    fn id(&self) -> &str;

    /// Settings-derived model catalog that overrides the static list
    /// registered with the provider in `ProviderInfo`.
    ///
    /// Default `None` ⇒ the registry serves the static list captured at
    /// init. A provider whose model set depends on runtime settings — the
    /// Ollama provider, where the user can register extra models by name —
    /// returns `Some(models)` so `/api/models` reflects a settings change
    /// without a restart. Called only on the read-only catalog path (the
    /// `/api/models` route and the MCP `list_models` tool), never on the
    /// dispatch hot path, so a DB read here is fine.
    async fn dynamic_models(&self) -> Option<Vec<ModelInfo>> {
        None
    }

    /// Published price of `model_id` in USD per million tokens as
    /// `(input, output)`, as defined by this provider. `None` means the
    /// provider carries no price for the model — callers must treat that as
    /// unknown, NOT free. Local providers that bill nothing return
    /// `Some((0.0, 0.0))`. Lets callers rank a provider's models by price
    /// (see `ProviderRegistry::cheapest_model`) without hardcoding model
    /// names outside the provider.
    fn model_price(&self, _model_id: &str) -> Option<(f64, f64)> {
        None
    }
    /// Begin an agent run for `ctx.session_id`. Returning Ok means the run
    /// has been scheduled; the actual streaming happens in a background
    /// task. Errors should be returned synchronously (e.g. spawn failure).
    async fn send_message(&self, ctx: SendMessageContext) -> anyhow::Result<()>;

    /// Cancel any in-flight run for `session_id`. Typically a hard kill.
    async fn cancel(&self, session_id: &str);

    /// Request a graceful exit for `session_id` once its current turn
    /// finishes. The caller does NOT block on completion.
    ///
    /// The contract: after `shutdown_after_turn` returns, the provider
    /// guarantees that any in-flight tool response can still reach the
    /// agent, the agent can emit any post-tool assistant text and a
    /// final `result` event, AND no `Crashed { reason: "interrupted" }`
    /// event will be appended on the way out. Providers whose runs are
    /// per-turn tasks that self-terminate on completion (mock, ollama)
    /// satisfy this contract trivially and can keep the default no-op.
    /// Providers that own a long-lived child (Claude CLI in stream-json
    /// mode) must arrange to close the child's stdin AFTER the current
    /// turn's `result` has been observed, so the child sees EOF and
    /// exits naturally.
    ///
    /// This is the *correct* way for the MCP terminal-step tools
    /// (`finish_card`, `complete_step`, `wont_do_card`) to stop a
    /// worker once its card has transitioned — using `cancel` there
    /// races the tool response and surfaces as a worker crash in the
    /// UI even though the transition itself succeeded.
    async fn shutdown_after_turn(&self, _session_id: &str) {}

    /// Stop the in-flight run for `session_id`. Implementations MUST actually
    /// terminate the run (kill the process / abort the task) — there is no
    /// "soft interrupt" path because the Claude CLI in stream-json mode does
    /// not respond to stdin signals. The difference from `cancel` is purely
    /// reporting: the route handler appends an `interrupt` event so the UI
    /// distinguishes a user interrupt from other cancellations.
    async fn interrupt(&self, session_id: &str);

    /// Deliver text to the run's input channel (e.g. an answer to a
    /// `ControlRequest`). Returns true if delivery was attempted.
    async fn write_stdin(&self, session_id: &str, text: &str) -> bool;

    /// Whether a run is currently in flight for this session.
    async fn is_running(&self, session_id: &str) -> bool;

    /// Block until any background run for `session_id` has fully wound down
    /// — including emitting any synthetic agent-end / Crashed event from the
    /// cancel path. Returns immediately if no run is tracked.
    ///
    /// Callers that wipe persistent state after cancelling (e.g. the
    /// `/clear` route) must call this between `cancel` and the wipe;
    /// otherwise the synthetic Crashed event lands AFTER the wipe and
    /// resurrects an "Agent crashed (interrupted)" line that the user
    /// just tried to delete.
    ///
    /// Default implementation: return immediately. Providers that own a
    /// background streaming task override this to poll their per-session
    /// tracking until the entry disappears.
    async fn wait_for_termination(&self, _session_id: &str) {}

    /// Whether `send_message` is safe to call again while a turn is
    /// already in flight.
    ///
    /// `true` ⇒ a second `send_message` mid-turn is consumed by the
    /// same long-lived run (e.g. the Claude CLI in stream-json mode
    /// buffers user envelopes on stdin and consumes them after the
    /// current `result`). The SessionManager dispatches such messages
    /// straight through, without a DB-level queue.
    ///
    /// `false` ⇒ the provider would spawn a parallel run, so the
    /// SessionManager falls back to persisting the message in the
    /// `queued_messages` table and draining it on completion. This
    /// is the mock provider's contract, and the contract callers
    /// expect for any future provider that can only handle one turn
    /// at a time.
    ///
    /// Default: `false` — the safe assumption is "treat this like a
    /// per-turn batch process unless the provider explicitly opts
    /// in." Override to `true` only when concurrent dispatch is the
    /// intended fast path.
    fn supports_mid_stream_injection(&self) -> bool {
        false
    }

    /// Drop any stale per-session state (e.g. exited processes).
    async fn cleanup(&self);

    /// Tear down all in-flight runs. Called on graceful shutdown.
    async fn shutdown(&self);
}

/// Persist a `ProviderEvent` to the database and broadcast it via WebSocket.
///
/// Shared by all providers so the event log + broadcast path is identical
/// regardless of which agent kind produced the event. Also persists
/// `conversation_id` on the session row when `Started` / `Completed` carry
/// one, so resume-by-conversation-id works across restarts.
pub async fn emit_event(
    db: &Db,
    broadcaster: &Broadcaster,
    session_id: &str,
    event: ProviderEvent,
) {
    let kind = event.event_kind().to_string();
    let data = event.event_data();

    match db.append_event(session_id, &kind, data.clone()).await {
        Ok(db_event) => {
            let now = chrono::Utc::now().to_rfc3339();
            let _ = db
                .update_session(
                    session_id,
                    crate::db::models::UpdateSession {
                        last_activity: Some(now),
                        ..Default::default()
                    },
                )
                .await;

            // Mirror `todo` snapshots into the dedicated `todos` table —
            // the source of truth for the load-time read path. The event
            // still wins for live WS updates, but a reload reads from
            // the table.
            if let ProviderEvent::Todo { ref todos } = event {
                let snapshot = crate::todo::TodoSnapshot {
                    todos: todos.clone(),
                };
                if let Err(e) = db.replace_session_todos(session_id, snapshot).await {
                    tracing::error!(
                        session_id = session_id,
                        "Failed to persist todo snapshot: {}",
                        e
                    );
                }
            }

            // Mirror per-turn token usage into the dedicated `usage_events`
            // table — the source of truth for usage/analytics rollups. The
            // event still drives live WS updates; the table is what the
            // aggregation queries read. Same colocation as the `todo`
            // mirror above. `db_event` gives us the originating event id
            // and the server-stamped ts.
            if let ProviderEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
                total_tokens,
                context_tokens,
                ref model,
                turn_seq,
            } = event
            {
                // Attribute the turn to the Claude account it billed
                // against, parsed from the `@<account_id>` suffix on the
                // session's model id. `None` for the implicit Default
                // account (host credentials) or any non-Claude provider.
                let account_id = match db.get_session(session_id).await {
                    Ok(Some(s)) => s.model.as_deref().and_then(|m| {
                        crate::provider::registry::split_model_account(m)
                            .1
                            .map(str::to_string)
                    }),
                    _ => None,
                };
                let new_usage = crate::db::models::NewUsageEvent {
                    id: uuid::Uuid::new_v4().to_string(),
                    session_id: session_id.to_string(),
                    event_id: Some(db_event.id.clone()),
                    turn_seq,
                    ts: db_event.ts,
                    input_tokens,
                    output_tokens,
                    cache_read_tokens,
                    cache_creation_tokens,
                    total_tokens,
                    context_tokens,
                    model: model.clone(),
                    account_id,
                };
                if let Err(e) = db.record_usage_event(new_usage).await {
                    tracing::error!(
                        session_id = session_id,
                        "Failed to persist usage_event: {}",
                        e
                    );
                }
            }

            if let ProviderEvent::Completed {
                conversation_id: Some(ref cid),
            } = event
            {
                let _ = db
                    .update_session(
                        session_id,
                        crate::db::models::UpdateSession {
                            conversation_id: Some(Some(cid.clone())),
                            ..Default::default()
                        },
                    )
                    .await;
            }

            if let ProviderEvent::Started {
                conversation_id: Some(ref cid),
                ..
            } = event
            {
                let _ = db
                    .update_session(
                        session_id,
                        crate::db::models::UpdateSession {
                            conversation_id: Some(Some(cid.clone())),
                            ..Default::default()
                        },
                    )
                    .await;
            }

            broadcaster.broadcast(WsEvent {
                event_type: "event".into(),
                session_id: session_id.to_string(),
                data: serde_json::json!({
                    "id": db_event.id,
                    "seq": db_event.seq,
                    "ts": db_event.ts,
                    "kind": db_event.kind,
                    "data": serde_json::from_str::<serde_json::Value>(&db_event.data).unwrap_or_default(),
                }),
            });

            // Lifecycle notification hooks — fire-and-forget after the event
            // is persisted and broadcast. No cancel semantics; plugin work
            // runs in a background task via `dispatch_notify`.
            match &event {
                ProviderEvent::Completed { .. } => {
                    crate::plugin::notify::fire_session_agent_ended(
                        db,
                        session_id,
                        "completed",
                        None,
                    )
                    .await;
                }
                ProviderEvent::Crashed { reason, .. } => {
                    crate::plugin::notify::fire_session_agent_ended(
                        db,
                        session_id,
                        "crashed",
                        Some(reason.as_str()),
                    )
                    .await;
                }
                ProviderEvent::ControlRequest {
                    request_type,
                    payload,
                    ..
                } if request_type == "question" => {
                    let preview: String = payload
                        .get("text")
                        .or_else(|| payload.get("question"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .chars()
                        .take(200)
                        .collect();
                    let (session_name, _) = match db.get_session(session_id).await {
                        Ok(Some(s)) => (s.name, s.is_worker),
                        _ => (session_id.to_string(), false),
                    };
                    let p = crate::plugin::notify::question_pending_payload(
                        session_id,
                        &session_name,
                        &preview,
                    );
                    crate::plugin::manager::notify(crate::plugin::hooks::QUESTION_PENDING_HOOK, p);
                }
                _ => {}
            }
        }
        Err(e) => {
            tracing::error!(
                session_id = session_id,
                kind = %kind,
                "Failed to persist event: {}",
                e
            );
        }
    }
}
