//! Provider-agnostic todo/task lifecycle hook for Extism plugins.
//!
//! The built-in Claude provider parses its `TodoWrite` tool calls into the
//! canonical [`TodoSnapshot`](crate::todo::TodoSnapshot) and emits them as
//! `todo` [`ProviderEvent`]s (see `src/provider/claude/process.rs`). This
//! module gives that exact same capability to *any other* provider through the
//! plugin system: a plugin parses its provider's native output however it
//! likes and hands back a normalized todo snapshot, which is emitted as the
//! identical `todo` event. Downstream consumers (the `/todos` route, the
//! frontend) never learn which agent produced the snapshot.
//!
//! The host seam is [`emit_plugin_todos`]: a provider calls it with the raw
//! output it just produced; if a `todo`-hook plugin turns that output into a
//! snapshot, it lands in the event log + WebSocket broadcast via the shared
//! [`emit_event`] path, exactly like the Claude todos do.

use crate::db::Db;
use crate::provider::agent::emit_event;
use crate::provider::stream::ProviderEvent;
use crate::todo::TodoSnapshot;
use crate::ws::broadcaster::Broadcaster;

use super::manager::PluginManager;

/// The hook name a plugin declares in its manifest to drive todo/task
/// lifecycle tracking. A plugin whose `manifest.hooks` contains `"todo"` is
/// invoked once per batch of raw provider output and asked to report the
/// session's current work items.
///
/// ## Hook contract
///
/// Like every hook, the call envelope is
/// `{ "hook": "todo", "payload": <raw provider output> }` and the plugin
/// replies with a standard [`Verdict`](super::hooks::Verdict). For the `todo`
/// hook the verdict carries the work items:
///
/// * **Report a snapshot** — return
///   `{ "verdict": "allow", "payload": { "todos": [ { "content": "...",
///   "status": "...", "activeForm": "..." }, ... ] } }`.
///   This is a *full replace-all* snapshot of every work item the session is
///   tracking, mirroring Claude's `TodoWrite` semantics — not a delta. A
///   single item transition (`Pending → In Progress → Done`) is expressed by
///   returning the whole list with that one item's `status` changed. The
///   `status` field is a provider-native token normalized via
///   [`TodoStatus::from_provider`](crate::todo::TodoStatus::from_provider)
///   (`pending` / `in_progress` / `completed` / `done`; anything unknown
///   degrades to `pending` rather than dropping the item). `activeForm` is
///   optional. An explicitly empty `"todos": []` is a valid snapshot meaning
///   "the agent cleared its list".
/// * **Nothing to report** — return `{ "verdict": "skip" }` (or omit a `todos`
///   array). No `todo` event is emitted for this batch.
/// * **Cancel** — `{ "verdict": "cancel", "reason": "..." }` is honored like
///   any hook, though it is unusual for todo tracking.
///
/// The raw payload the host supplies is opaque, provider-native output that the
/// plugin alone knows how to parse; the host never inspects it. Only an
/// explicit `allow` payload carrying a `todos` array produces an event.
pub const TODO_HOOK: &str = "todo";

/// Parse the payload a `todo`-hook plugin returned in its `allow` verdict into
/// the canonical snapshot.
///
/// Reuses [`TodoSnapshot::from_todo_write_input`] so a plugin-reported snapshot
/// is normalized byte-for-byte identically to the Claude `TodoWrite` path —
/// same status mapping, same `activeForm` handling. Returns `None` when the
/// payload carries no `todos` array (e.g. the plugin skipped or returned an
/// unrelated shape), so the caller emits no `todo` event.
pub fn snapshot_from_plugin_payload(payload: &serde_json::Value) -> Option<TodoSnapshot> {
    TodoSnapshot::from_todo_write_input(payload)
}

impl PluginManager {
    /// Run one batch of raw provider output through any `todo`-hook plugins and
    /// return the normalized snapshot a plugin produced, if any.
    ///
    /// Short-circuits to `None` when no plugin handles [`TODO_HOOK`], so
    /// providers can call this unconditionally on every batch with no cost when
    /// no todo plugin is installed. See [`TODO_HOOK`] for the wire contract.
    pub async fn dispatch_todo(&self, raw_output: serde_json::Value) -> Option<TodoSnapshot> {
        if !self.has_listeners(TODO_HOOK).await {
            return None;
        }
        let payload = self.dispatch(TODO_HOOK, raw_output).await.into_payload()?;
        snapshot_from_plugin_payload(&payload)
    }
}

/// Host seam a non-Claude provider calls to let a plugin drive todo lifecycle
/// tracking for one batch of its output.
///
/// Dispatches `raw_output` to the `todo` hook and, if a plugin reported a
/// snapshot, emits it as the canonical [`ProviderEvent::Todo`] through the
/// shared [`emit_event`] persistence + broadcast path — the *same* event the
/// built-in Claude path produces. Returns `true` when an event was emitted.
/// A no-op (returns `false`) when no `todo`-hook plugin is installed or a
/// plugin reported nothing, so it is safe to call on every turn.
pub async fn emit_plugin_todos(
    plugins: &PluginManager,
    db: &Db,
    broadcaster: &Broadcaster,
    session_id: &str,
    raw_output: serde_json::Value,
) -> bool {
    match plugins.dispatch_todo(raw_output).await {
        Some(snapshot) => {
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::Todo {
                    todos: snapshot.todos,
                },
            )
            .await;
            true
        }
        None => false,
    }
}
