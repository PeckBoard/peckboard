//! Delivering a message into a session *as if a user had typed it*.
//!
//! This is the shared, app-independent half of the expert-messaging path:
//! anything that wants an expert / session to receive a message the same way a
//! user message arrives goes through [`persist_user_message`]. It appends a
//! `user` event (tagged with a `source` so the UI / later readers know it
//! wasn't literally typed) and broadcasts it as a normal `event`, exactly like
//! the HTTP message route does before it dispatches.
//!
//! Persisting is decoupled from *resuming*: [`persist_user_message`] works with
//! no running app (tests / headless / early boot), leaving the message durable
//! and visible on the session's next run. The resume half — spawning or
//! queueing an agent turn — lives behind the `ExpertDispatcher::resume_session`
//! seam, because only the live `AppState` can reach the `SessionManager`.

use std::sync::Arc;

use crate::db::Db;
use crate::ws::broadcaster::{Broadcaster, WsEvent};

/// Append `text` to `session_id`'s log as a `user` event tagged with `source`,
/// and broadcast it so any live UI renders it like a user-typed message.
///
/// Mirrors the up-front append + broadcast the HTTP message route performs
/// (`routes/sessions/dispatch.rs`). It does NOT resume the session — callers
/// pair it with `ExpertDispatcher::resume_session` when a live app is present.
pub async fn persist_user_message(
    db: &Db,
    broadcaster: &Arc<Broadcaster>,
    session_id: &str,
    text: &str,
    source: &str,
) -> anyhow::Result<()> {
    let event = db
        .append_event(
            session_id,
            "user",
            serde_json::json!({ "text": text, "source": source }),
        )
        .await?;

    broadcaster.broadcast(WsEvent {
        event_type: "event".into(),
        session_id: session_id.to_string(),
        data: serde_json::json!({
            "id": event.id,
            "seq": event.seq,
            "ts": event.ts,
            "kind": event.kind,
            "data": serde_json::from_str::<serde_json::Value>(&event.data).unwrap_or_default(),
        }),
    });

    Ok(())
}
