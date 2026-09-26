//! Deliver a server-originated report into a session as if the user had
//! typed it: persist a durable `user` event (tagged so the UI can render it
//! distinctly), broadcast it live, then wake the session — a fresh turn when
//! idle, or the durable queue when a turn is in flight.
//!
//! Shared by subagent completion reports (`crate::subagent`) and background
//! task reports (`crate::background`).

use crate::db::Db;
use crate::service::mcp_server::ExpertDispatcher;
use crate::ws::broadcaster::{Broadcaster, WsEvent};

/// Append `text` to `session_id` as a `user` event whose data is
/// `{"text": text, ..marker}` (every key of the `marker` object is merged in,
/// e.g. `{"source": "subagent-result"}`), broadcast it as a regular `event`
/// frame, then resume the session through `dispatcher` (skipped when `None`,
/// e.g. at boot or in tests — the persisted event is still picked up on the
/// session's next turn).
///
/// If the event can't be persisted the session is still resumed (via plain
/// `resume_session`, so a queued delivery appends the user event itself) —
/// a report must never be dropped because one DB write failed — and the
/// append error is returned for the caller to log. A delivery failure is
/// logged and swallowed.
pub async fn notify_session(
    db: &Db,
    broadcaster: &Broadcaster,
    dispatcher: Option<&dyn ExpertDispatcher>,
    session_id: &str,
    text: &str,
    marker: serde_json::Value,
) -> anyhow::Result<()> {
    let mut data = serde_json::json!({ "text": text });
    if let (Some(obj), serde_json::Value::Object(extra)) = (data.as_object_mut(), marker) {
        for (k, v) in extra {
            obj.insert(k, v);
        }
    }
    let appended = db.append_event(session_id, "user", data.clone()).await;
    if let Ok(event) = &appended {
        broadcaster.broadcast(WsEvent {
            event_type: "event".into(),
            session_id: session_id.to_string(),
            data: serde_json::json!({
                "id": event.id,
                "seq": event.seq,
                "ts": event.ts,
                "kind": event.kind,
                "data": data,
            }),
        });
    }

    if let Some(dispatcher) = dispatcher {
        let resumed = if appended.is_ok() {
            dispatcher.resume_session_appended(session_id, text).await
        } else {
            dispatcher.resume_session(session_id, text).await
        };
        if let Err(e) = resumed {
            tracing::warn!(
                session_id = %session_id,
                "session notification delivery failed (the session sees the persisted event on its next turn): {e}"
            );
        }
    }
    appended.map(|_| ())
}
