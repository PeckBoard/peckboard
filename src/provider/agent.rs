use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::db::Db;
use crate::plugin::manager::PluginManager;
use crate::provider::stream::{ProviderEvent, SpawnConfig};
use crate::ws::broadcaster::{Broadcaster, WsEvent};

/// Notification sent when an agent run finishes streaming.
///
/// Delivered to the dispatcher's completion channel so the worker
/// orchestrator can react (e.g. advance a card) outside the streaming
/// task itself.
pub struct ProcessCompletion {
    pub session_id: String,
    pub completed: bool,
}

/// Context handed to an `AgentProvider` when a new run is requested.
///
/// The dispatcher resolves the working directory, the (optional) resume
/// `conversation_id`, and the completion channel so individual providers
/// don't need to repeat that bookkeeping.
pub struct SendMessageContext {
    pub session_id: String,
    pub message: String,
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

    /// Begin an agent run for `ctx.session_id`. Returning Ok means the run
    /// has been scheduled; the actual streaming happens in a background
    /// task. Errors should be returned synchronously (e.g. spawn failure).
    async fn send_message(&self, ctx: SendMessageContext) -> anyhow::Result<()>;

    /// Cancel any in-flight run for `session_id`. Typically a hard kill.
    async fn cancel(&self, session_id: &str);

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
