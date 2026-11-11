//! Focus mode for chat sessions: while the user is actively talking to a
//! chat, a message another session sends it (`send_message` /
//! `send_image` from the session-control plugin) is held instead of
//! landing in the middle of the conversation.
//!
//! "Actively talking" means the user sent the chat a message within
//! [`CHAT_FOCUS_WINDOW`], or the agent's turn on that message is still
//! running. Held messages are released together, as one clearly labelled
//! turn, once the user goes quiet (the [`spawn_ticker`] loop), or at once
//! when the user presses "Show now" (`POST /api/sessions/:id/held/release`).
//!
//! Only chats with a human in them are gated: worker sessions, expert /
//! voice sessions, subagents and orchestrator brains (which take their
//! messages through `orchestrate_send`, not this path) are unaffected. The
//! voice Assistant has its own gate (`service::voice_gate`). A message with
//! attachments is never held (its bytes would need persisting first). Nor is
//! the user's own request: the voice Assistant relaying what the user said,
//! or an authenticated user request (the plugin host checks both).
//!
//! Held rows live in `chat_focus_queue` and are deleted on delivery, so
//! nothing is lost across a restart. The activity map is in-memory only: a
//! restart forgets who was talking, so held messages go out at the next
//! tick, which is the safe direction.
//!
//! Instance setting `focus_mode` (`{"enabled": bool}`, default on) turns it
//! off.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::db::Db;
use crate::db::models::{ChatFocusItem, Session};
use crate::routes::settings::{SETTINGS_COLLECTION, SETTINGS_NS};
use crate::state::AppState;
use crate::ws::broadcaster::{Broadcaster, WsEvent};

/// How long after the user's last message a chat counts as "in a
/// conversation".
pub const CHAT_FOCUS_WINDOW: Duration = Duration::from_secs(180);
/// How often the ticker looks for chats that went quiet.
const TICK: Duration = Duration::from_secs(2);
/// Settings key (`{"enabled": bool}`); missing means on.
pub const FOCUS_MODE_KEY: &str = "focus_mode";
/// WS event on the chat's stream when its held count changes: `{count}`.
pub const FOCUS_HELD_EVENT: &str = "focus-held";

#[derive(Default)]
struct Activity {
    last_user: Option<Instant>,
    /// The agent is answering the user's message.
    turn_open: bool,
}

static ACTIVITY: LazyLock<Mutex<HashMap<String, Activity>>> = LazyLock::new(Default::default);
/// Sessions whose held count changed since the last push.
static DIRTY: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

fn activity() -> std::sync::MutexGuard<'static, HashMap<String, Activity>> {
    ACTIVITY.lock().unwrap_or_else(|e| e.into_inner())
}

fn mark_dirty(session_id: &str) {
    DIRTY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(session_id.to_string());
}

/// The user sent `session_id` a message (the chat send route).
pub fn note_user_message(session_id: &str) {
    note_user_message_at(session_id, Instant::now());
}

pub fn note_user_message_at(session_id: &str, now: Instant) {
    let mut a = activity();
    let e = a.entry(session_id.to_string()).or_default();
    e.last_user = Some(now);
    e.turn_open = true;
}

/// A session's agent turn ended. A no-op lookup for sessions nobody talks to.
pub fn note_turn_end(session_id: &str) {
    if let Some(e) = activity().get_mut(session_id) {
        e.turn_open = false;
    }
}

/// Is the user in a conversation with `session_id` right now?
pub fn is_active_at(session_id: &str, now: Instant) -> bool {
    activity().get(session_id).is_some_and(|e| {
        e.turn_open
            || e.last_user
                .is_some_and(|t| now.saturating_duration_since(t) < CHAT_FOCUS_WINDOW)
    })
}

/// A chat a human talks to (see the module docs for what is excluded).
pub fn is_human_chat(s: &Session) -> bool {
    !s.is_worker
        && s.expert_kind.is_none()
        && !s.is_expert
        && s.parent_session_id.is_none()
        && s.sealed_at.is_none()
}

/// The `focus_mode` setting, read on the calling (blocking) thread.
pub fn enabled_blocking(db: &Db) -> bool {
    db.plugin_store_get_blocking(SETTINGS_NS, SETTINGS_COLLECTION, FOCUS_MODE_KEY)
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("enabled").and_then(|e| e.as_bool()))
        .unwrap_or(true)
}

/// Called by the plugin host before delivering another session's message
/// to `target`. `Some(reply)` means it was held (the reply goes back to the
/// sending agent instead of the usual `{ok}`); `None` means deliver now.
pub fn maybe_hold_blocking(
    db: &Db,
    target: &Session,
    caller_session_id: Option<&str>,
    text: &str,
    has_attachments: bool,
) -> Option<serde_json::Value> {
    if has_attachments || caller_session_id == Some(target.id.as_str()) || !is_human_chat(target) {
        return None;
    }
    if !is_active_at(&target.id, Instant::now()) || !enabled_blocking(db) {
        return None;
    }
    let source = caller_session_id.and_then(|id| db.get_session_blocking(id).ok().flatten());
    let item = ChatFocusItem {
        id: uuid::Uuid::new_v4().to_string(),
        session_id: target.id.clone(),
        source_session_id: source.as_ref().map(|s| s.id.clone()),
        source_name: source
            .as_ref()
            .map(|s| s.name.clone())
            .unwrap_or_else(|| "another session".to_string()),
        text: text.to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    if let Err(e) = db.insert_chat_focus_item_blocking(item) {
        // Failing open: better an interruption than a lost message.
        tracing::warn!(session_id = %target.id, "chat focus: holding failed, delivering now: {e}");
        return None;
    }
    tracing::info!(session_id = %target.id, "chat focus: held a message while the user is talking");
    mark_dirty(&target.id);
    Some(serde_json::json!({
        "ok": true,
        "held": true,
        "session_id": target.id,
        "note": "The user is in a conversation with that session right now, so your message is held (focus mode). It is delivered when they go quiet or ask for it; don't resend it.",
    }))
}

/// The one turn that carries every held message, labelled so the agent
/// treats it as separate from what it was just discussing.
pub fn format_release(items: &[ChatFocusItem]) -> String {
    let mut out = String::from(
        "[held while the user was talking to you] Messages other sessions sent in the meantime. \
         They are separate from the conversation you were having; handle them as their own topic.",
    );
    for it in items {
        let from = match &it.source_session_id {
            Some(id) => format!("\"{}\" (session {id})", it.source_name),
            None => format!("\"{}\"", it.source_name),
        };
        out.push_str(&format!("\n\nFrom {from}:\n{}", it.text));
    }
    out
}

/// Deliver everything held for `session_id` now, as one turn. Returns how
/// many messages went out.
///
/// The rows are dropped whatever the dispatch outcome: the message has been
/// handed to the session (its user event lands in the transcript, or it
/// joins the durable queue), so a failed agent start — a missing login, say
/// — must not keep it held and deliver it a second time later.
pub async fn release(state: &Arc<AppState>, session_id: &str) -> anyhow::Result<usize> {
    let items = state.db.list_chat_focus_items(session_id).await?;
    if items.is_empty() {
        return Ok(0);
    }
    let text = format_release(&items);
    let sent = crate::service::mcp_server::AppExpertDispatcher::new(state.clone())
        .send_message_with_attachments(session_id, text, Vec::new())
        .await;
    let ids = items.iter().map(|i| i.id.clone()).collect();
    state.db.delete_chat_focus_items(ids).await?;
    mark_dirty(session_id);
    announce(&state.db, &state.broadcaster).await;
    if let Err(e) = sent {
        tracing::warn!(session_id = %session_id, "chat focus: released messages, but the agent did not start: {e}");
    }
    Ok(items.len())
}

/// Push `{count}` to every chat whose held count changed.
async fn announce(db: &Db, broadcaster: &Broadcaster) {
    let dirty: Vec<String> = DIRTY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain()
        .collect();
    for session_id in dirty {
        let count = db
            .list_chat_focus_items(&session_id)
            .await
            .map(|v| v.len())
            .unwrap_or(0);
        broadcaster.broadcast(WsEvent {
            event_type: FOCUS_HELD_EVENT.into(),
            data: serde_json::json!({ "count": count }),
            session_id,
        });
    }
}

/// One pass: release every chat that went quiet, then push count changes.
pub async fn tick_at(state: &Arc<AppState>, now: Instant) {
    let sessions = state.db.chat_focus_sessions().await.unwrap_or_default();
    for session_id in sessions {
        if is_active_at(&session_id, now) {
            continue;
        }
        if let Err(e) = release(state, &session_id).await {
            tracing::warn!(session_id = %session_id, "chat focus: releasing held messages failed: {e}");
        }
    }
    announce(&state.db, &state.broadcaster).await;
}

/// Start the release loop. Called once at boot.
pub fn spawn_ticker(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            tick_at(&state, Instant::now()).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_while_the_turn_runs_and_for_the_window_after() {
        let t0 = Instant::now();
        note_user_message_at("cf-a", t0);
        assert!(
            is_active_at("cf-a", t0 + Duration::from_secs(600)),
            "turn still open"
        );
        note_turn_end("cf-a");
        assert!(is_active_at("cf-a", t0 + Duration::from_secs(60)));
        assert!(!is_active_at("cf-a", t0 + CHAT_FOCUS_WINDOW));
        assert!(!is_active_at("cf-never", t0));
    }

    #[test]
    fn release_text_labels_each_sender() {
        let item = |name: &str, sid: Option<&str>, text: &str| ChatFocusItem {
            id: name.into(),
            session_id: "s".into(),
            source_session_id: sid.map(str::to_string),
            source_name: name.into(),
            text: text.into(),
            created_at: "now".into(),
        };
        let text = format_release(&[
            item("MochiMask", Some("m1"), "board check done"),
            item("gone", None, "hi"),
        ]);
        assert!(text.starts_with("[held while the user was talking to you]"));
        assert!(text.contains("From \"MochiMask\" (session m1):\nboard check done"));
        assert!(text.contains("From \"gone\":\nhi"));
    }
}
