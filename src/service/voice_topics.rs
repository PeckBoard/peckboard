//! The voice assistant's topic ring, shown on the full-page Assistant view.
//!
//! The assistant model curates the whole ring through its `assistant_topics`
//! tool (`mcp_server::handlers::assistant_topics`): conversation threads and
//! the sessions / projects it chose to put there. Nothing is derived
//! automatically. Rows live in `voice_topics` (`db::crud::voice_topics`);
//! a session / project topic's live status is computed here at read time.
//!
//! Every change — tool or `POST /api/voice/topics/focus` — broadcasts
//! [`VOICE_TOPICS_EVENT`] on the voice session's stream with the full
//! [`payload`], so the page never refetches.

use serde_json::{Value, json};

use crate::db::Db;
use crate::db::models::VoiceTopic;
use crate::ws::broadcaster::{Broadcaster, WsEvent};

/// WS event on the voice session's stream: the ring changed. `data` is the
/// same `{currentId, topics}` as `GET /api/voice/topics`.
pub const VOICE_TOPICS_EVENT: &str = "voice-topics";

/// What the voice session is told when the user picks a topic on screen.
pub fn focus_message(title: &str) -> String {
    format!("Let's talk about {title}.")
}

/// A session's live status from its event tail: `finished` (sealed),
/// `waiting on you`, `running`, `crashed`, or `idle`. `None` when gone.
async fn session_status(db: &Db, id: &str) -> Option<String> {
    let session = db.get_session(id).await.ok()??;
    if session.sealed_at.is_some() {
        return Some("finished".into());
    }
    let tail = db.events_tail(id, 10).await.unwrap_or_default();
    Some(
        match crate::routes::sessions::derive_status(&tail) {
            "questioning" => "waiting on you",
            "working" | "tool-active" => "running",
            other => other,
        }
        .into(),
    )
}

/// A project's live status: `paused`, `waiting on you` (a blocked card),
/// `running` (a worker mid-turn), `finished` (every card done / won't do),
/// or `idle`. `None` when gone.
async fn project_status(db: &Db, id: &str) -> Option<String> {
    let project = db.get_project(id).await.ok()??;
    if project.status == "paused" {
        return Some("paused".into());
    }
    let cards = db.list_card_overviews(id).await.unwrap_or_default();
    if cards.iter().any(|c| c.blocked) {
        return Some("waiting on you".into());
    }
    for sid in cards.iter().filter_map(|c| c.worker_session_id.as_deref()) {
        if session_status(db, sid).await.as_deref() == Some("running") {
            return Some("running".into());
        }
    }
    if !cards.is_empty()
        && cards
            .iter()
            .all(|c| c.step == "done" || c.step == "wont_do")
    {
        return Some("finished".into());
    }
    Some("idle".into())
}

/// The live status of what a topic points at; `None` for conversation
/// topics, `gone` for a deleted target.
pub async fn live_status(db: &Db, topic: &VoiceTopic) -> Option<String> {
    let ref_id = topic.ref_id.as_deref()?;
    let status = match topic.kind.as_str() {
        "session" => session_status(db, ref_id).await,
        "project" => project_status(db, ref_id).await,
        _ => return None,
    };
    Some(status.unwrap_or_else(|| "gone".into()))
}

/// Refuse a `ref_id` that is not an existing session / project. The voice
/// session is global (admin-only) and may reach every session in every
/// folder, so existence is the visibility check; it may not point at
/// itself.
pub async fn check_ref(db: &Db, voice_id: &str, kind: &str, ref_id: &str) -> anyhow::Result<()> {
    match kind {
        "session" => {
            if ref_id == voice_id || db.get_session(ref_id).await?.is_none() {
                anyhow::bail!(
                    "no session with id '{ref_id}'; find it with find_session or list_sessions"
                );
            }
        }
        "project" => {
            if db.get_project(ref_id).await?.is_none() {
                anyhow::bail!("no project with id '{ref_id}'; find it with list_projects");
            }
        }
        _ => {}
    }
    Ok(())
}

/// `{currentId, topics: [AssistantTopic]}` — the page's shape
/// (`web/src/voice/assistantContract.ts`): open topics only, newest change
/// first, camelCase.
pub async fn payload(db: &Db, voice_id: &str) -> anyhow::Result<Value> {
    let rows = db.list_open_voice_topics(voice_id).await?;
    let mut topics = Vec::with_capacity(rows.len());
    for t in &rows {
        topics.push(json!({
            "id": t.id,
            "kind": t.kind,
            "title": t.title,
            "summary": t.summary,
            "refId": t.ref_id,
            "status": live_status(db, t).await,
            "updatedAt": t.updated_at,
        }));
    }
    let current = rows.iter().find(|t| t.is_current).map(|t| t.id.clone());
    Ok(json!({ "currentId": current, "topics": topics }))
}

/// Push the ring to every browser on the voice session's stream.
pub async fn broadcast(db: &Db, broadcaster: &Broadcaster, voice_id: &str) {
    match payload(db, voice_id).await {
        Ok(data) => broadcaster.broadcast(WsEvent {
            event_type: VOICE_TOPICS_EVENT.into(),
            session_id: voice_id.to_string(),
            data,
        }),
        Err(e) => tracing::warn!(voice_session = %voice_id, "voice topics broadcast: {e}"),
    }
}

/// Keep the relay gate (`service::voice_gate`) on the focused topic's
/// target, so its relays are delivered rather than held as off-topic.
pub async fn focus_gate(db: &Db, voice_id: &str, topic: &VoiceTopic) {
    match (topic.kind.as_str(), topic.ref_id.as_deref()) {
        ("session", Some(id)) => crate::service::voice_gate::focus_session(db, voice_id, id).await,
        ("project", Some(id)) => crate::service::voice_gate::focus_project(voice_id, id),
        _ => {}
    }
}
