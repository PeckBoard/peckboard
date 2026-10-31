//! Topic boundaries and the Assistant's automatic context clear.
//!
//! Boundaries:
//! - **close** — `assistant_topics close`: the topic ends.
//! - **switch** — `assistant_topics focus` on another topic: the previous
//!   topic is *parked*. It may get memory, but nothing is cleared.
//! - **idle** — no user utterance for `idle_topic_end_minutes` while a topic
//!   is current: that topic ends like a close (fallback memory + clear), but
//!   it stays open and current on the ring — the user may well come back to
//!   it, and only the conversation context is dropped. It ends once per
//!   quiet stretch: the next utterance re-arms it.
//!
//! An ended topic with `auto_clear = after_topic` schedules a clear at
//! `now + clear_delay_minutes`. [`decide`] (pure) lets it fire only when no
//! Assistant turn runs, the user isn't speaking, no reply is being read
//! aloud, no relay about the ended topic is held, and nothing else
//! happened since: a user utterance or a focus on another topic cancels it.
//! The fire ([`fire`]) keeps memory, clears the session, rebuilds the prompt
//! with the Recent Memory snapshot, and posts a `system` event.
//!
//! State is in memory, per voice session: a restart forgets a pending clear
//! (and treats boot as fresh activity), which only means one fewer clear.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::service::assistant_settings::{self, AssistantSettings, AutoClear};
use crate::service::voice_relay::VOICE_EXPERT_KIND;
use crate::state::AppState;

/// How often pending clears and idle topics are evaluated.
const TICK: StdDuration = StdDuration::from_secs(5);
/// How often expired memories are swept (reads sweep too).
const SWEEP_EVERY: StdDuration = StdDuration::from_secs(600);

/// `data.kind` of the `system` event a clear posts.
pub const CLEARED_EVENT_KIND: &str = "assistant-context-cleared";

/// Why a topic ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Boundary {
    Close,
    Idle,
}

/// A topic that ended and is waiting for the clear.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EndedTopic {
    pub id: String,
    pub title: String,
    pub summary: Option<String>,
    /// `session` / `project` topics: the relay gate's held relays about
    /// this target hold the clear back.
    pub kind: String,
    pub ref_id: Option<String>,
}

impl EndedTopic {
    pub fn of(t: &crate::db::models::VoiceTopic) -> Self {
        Self {
            id: t.id.clone(),
            title: t.title.clone(),
            summary: t.summary.clone(),
            kind: t.kind.clone(),
            ref_id: t.ref_id.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PendingClear {
    pub scheduled_at: DateTime<Utc>,
    pub due: DateTime<Utc>,
    pub reason: Boundary,
    pub topics: Vec<EndedTopic>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LastClear {
    pub at: DateTime<Utc>,
    pub reason: Boundary,
    pub topic_title: String,
    pub kept: usize,
}

struct Life {
    last_utterance: DateTime<Utc>,
    last_focus_switch: Option<DateTime<Utc>>,
    pending: Option<PendingClear>,
    last_clear: Option<LastClear>,
    /// The topic already idle-ended in this quiet stretch.
    idle_ended: Option<String>,
    /// Last logged wait reason, so a steady wait logs once.
    last_wait: Option<&'static str>,
}

impl Life {
    /// A fresh state (first use, or after a restart) counts as activity.
    fn new(now: DateTime<Utc>) -> Self {
        Self {
            last_utterance: now,
            last_focus_switch: None,
            pending: None,
            last_clear: None,
            idle_ended: None,
            last_wait: None,
        }
    }
}

static LIVES: LazyLock<Mutex<HashMap<String, Life>>> = LazyLock::new(Default::default);

fn with_life<R>(voice_id: &str, now: DateTime<Utc>, f: impl FnOnce(&mut Life) -> R) -> R {
    let mut all = LIVES.lock().unwrap_or_else(|e| e.into_inner());
    f(all
        .entry(voice_id.to_string())
        .or_insert_with(|| Life::new(now)))
}

/// The user said (or typed) something to the Assistant: cancels a pending
/// clear and re-arms the idle boundary.
pub fn note_user_utterance(voice_id: &str) {
    note_user_utterance_at(voice_id, Utc::now());
}

pub fn note_user_utterance_at(voice_id: &str, now: DateTime<Utc>) {
    with_life(voice_id, now, |l| {
        l.last_utterance = now;
        l.idle_ended = None;
        if let Some(p) = l.pending.take() {
            tracing::info!(voice_session = %voice_id, topics = p.topics.len(), "assistant memory: pending clear cancelled (user spoke)");
        }
    });
}

/// The conversation switched to another topic (the previous one is parked).
pub fn note_focus_switch_at(voice_id: &str, now: DateTime<Utc>) {
    with_life(voice_id, now, |l| l.last_focus_switch = Some(now));
}
/// `session_id` was cleared (by hand, or by [`fire`]): a pending automatic
/// clear has nothing left to do. A no-op lookup for non-voice sessions.
pub fn cancel_pending(session_id: &str) {
    let mut all = LIVES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = all.get_mut(session_id).and_then(|l| l.pending.take()) {
        tracing::info!(voice_session = %session_id, topics = p.topics.len(), "assistant memory: pending clear dropped (session cleared)");
    }
}

/// `topic` ended: schedule (or re-arm) the clear when auto-clear is on.
pub fn topic_ended_at(
    voice_id: &str,
    topic: EndedTopic,
    reason: Boundary,
    settings: &AssistantSettings,
    now: DateTime<Utc>,
) {
    if settings.auto_clear == AutoClear::Off {
        tracing::info!(voice_session = %voice_id, topic = %topic.title, ?reason, "assistant memory: topic ended, auto-clear is off");
        return;
    }
    let due = now + Duration::minutes(i64::from(settings.clear_delay_minutes));
    with_life(voice_id, now, |l| {
        let mut topics = l.pending.take().map(|p| p.topics).unwrap_or_default();
        topics.retain(|t| t.id != topic.id);
        tracing::info!(voice_session = %voice_id, topic = %topic.title, ?reason, %due, "assistant memory: clear scheduled");
        topics.push(topic);
        l.pending = Some(PendingClear {
            scheduled_at: now,
            due,
            reason,
            topics,
        });
        l.last_wait = None;
    });
}

/// The inputs of one auto-clear decision.
#[derive(Clone, Debug)]
pub struct ClearInputs {
    pub now: DateTime<Utc>,
    pub auto_clear: AutoClear,
    pub scheduled_at: DateTime<Utc>,
    pub due: DateTime<Utc>,
    pub last_utterance: DateTime<Utc>,
    pub last_focus_switch: Option<DateTime<Utc>>,
    /// An Assistant turn is running (or still owed to the user).
    pub turn_running: bool,
    pub user_speaking: bool,
    pub tts_playing: bool,
    /// A held relay is about one of the ended topics.
    pub held_relay_for_topic: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClearDecision {
    Fire,
    /// Not yet; check again on the next tick.
    Wait(&'static str),
    /// Drop the pending clear.
    Cancel(&'static str),
}

/// Whether a pending clear fires now. Pure: everything comes in `i`.
pub fn decide(i: &ClearInputs) -> ClearDecision {
    if i.auto_clear == AutoClear::Off {
        return ClearDecision::Cancel("auto-clear is off");
    }
    if i.last_utterance > i.scheduled_at {
        return ClearDecision::Cancel("the user spoke");
    }
    if i.last_focus_switch.is_some_and(|t| t > i.scheduled_at) {
        return ClearDecision::Cancel("another topic was focused");
    }
    if i.now < i.due {
        return ClearDecision::Wait("delay not elapsed");
    }
    if i.turn_running {
        return ClearDecision::Wait("an Assistant turn is running");
    }
    if i.user_speaking {
        return ClearDecision::Wait("the user is speaking");
    }
    if i.tts_playing {
        return ClearDecision::Wait("a reply is being read aloud");
    }
    if i.held_relay_for_topic {
        return ClearDecision::Wait("a relay about the topic is held");
    }
    ClearDecision::Fire
}

/// Whether the current topic has been quiet long enough to end.
pub fn idle_due(now: DateTime<Utc>, last_utterance: DateTime<Utc>, idle_minutes: u32) -> bool {
    now - last_utterance >= Duration::minutes(i64::from(idle_minutes))
}

/// `{pending: {scheduled_at, due_at, reason, topics} | null, last_clear:
/// {at, reason, topic_title, kept} | null}` for `GET /api/voice/memory/status`.
pub fn status(voice_id: &str) -> serde_json::Value {
    let all = LIVES.lock().unwrap_or_else(|e| e.into_inner());
    let life = all.get(voice_id);
    let pending = life.and_then(|l| l.pending.as_ref()).map(|p| {
        serde_json::json!({
            "scheduled_at": p.scheduled_at.to_rfc3339(),
            "due_at": p.due.to_rfc3339(),
            "reason": p.reason,
            "topics": p.topics.iter().map(|t| t.title.clone()).collect::<Vec<_>>(),
        })
    });
    let last = life.and_then(|l| l.last_clear.as_ref()).map(|c| {
        serde_json::json!({
            "at": c.at.to_rfc3339(),
            "reason": c.reason,
            "topic_title": c.topic_title,
            "kept": c.kept,
        })
    });
    serde_json::json!({ "pending": pending, "last_clear": last })
}

/// `topic` ended (close or idle): fallback memory when nothing was
/// recorded for it, then the clear is scheduled.
pub async fn end_topic(
    db: &crate::db::Db,
    voice_id: &str,
    topic: EndedTopic,
    reason: Boundary,
    settings: &AssistantSettings,
    now: DateTime<Utc>,
) {
    if let Err(e) = super::write_fallback(
        db,
        voice_id,
        settings,
        &topic.id,
        &topic.title,
        topic.summary.as_deref(),
        now,
    )
    .await
    {
        tracing::warn!(voice_session = %voice_id, topic = %topic.title, "assistant memory: fallback write failed: {e}");
    }
    topic_ended_at(voice_id, topic, reason, settings, now);
}

/// Evaluate the voice session's idle boundary and pending clear at `now`.
pub async fn tick_at(state: &Arc<AppState>, now: DateTime<Utc>) {
    let Ok(Some(voice)) = state.db.find_expert_session(VOICE_EXPERT_KIND).await else {
        return;
    };
    let voice_id = voice.id.as_str();
    let settings = assistant_settings::load(&state.db).await;

    // Idle boundary.
    let current = state
        .db
        .list_open_voice_topics(voice_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .find(|t| t.is_current);
    if let Some(topic) = current {
        let ends = with_life(voice_id, now, |l| {
            let due = idle_due(now, l.last_utterance, settings.idle_topic_end_minutes)
                && l.idle_ended.as_deref() != Some(topic.id.as_str());
            if due {
                l.idle_ended = Some(topic.id.clone());
            }
            due
        });
        if ends {
            tracing::info!(voice_session = %voice_id, topic = %topic.title, "assistant memory: topic ended by inactivity");
            end_topic(
                &state.db,
                voice_id,
                EndedTopic::of(&topic),
                Boundary::Idle,
                &settings,
                now,
            )
            .await;
        }
    }

    // Pending clear.
    let Some((pending, last_utterance, last_focus_switch)) = with_life(voice_id, now, |l| {
        l.pending
            .clone()
            .map(|p| (p, l.last_utterance, l.last_focus_switch))
    }) else {
        return;
    };
    let manager = &state.session_manager;
    let running = manager.is_running(voice_id).await && !manager.is_lingering(voice_id).await;
    let gate = crate::service::voice_gate::conversation_state(voice_id);
    let held_relay_for_topic = pending.topics.iter().any(|t| {
        let Some(r) = t.ref_id.as_deref() else {
            return false;
        };
        gate.held.iter().any(|(s, p)| match t.kind.as_str() {
            "session" => s.as_deref() == Some(r),
            "project" => p.as_deref() == Some(r),
            _ => false,
        })
    });
    let decision = decide(&ClearInputs {
        now,
        auto_clear: settings.auto_clear,
        scheduled_at: pending.scheduled_at,
        due: pending.due,
        last_utterance,
        last_focus_switch,
        turn_running: running || gate.answering,
        user_speaking: gate.user_speaking,
        tts_playing: gate.tts_playing,
        held_relay_for_topic,
    });
    match decision {
        ClearDecision::Wait(why) => with_life(voice_id, now, |l| {
            if l.last_wait != Some(why) {
                tracing::info!(voice_session = %voice_id, why, "assistant memory: clear waiting");
                l.last_wait = Some(why);
            }
        }),
        ClearDecision::Cancel(why) => {
            tracing::info!(voice_session = %voice_id, why, "assistant memory: pending clear cancelled");
            with_life(voice_id, now, |l| {
                if l.pending.as_ref() == Some(&pending) {
                    l.pending = None;
                }
            });
        }
        ClearDecision::Fire => {
            // Claim it first, so a slow clear can't fire twice.
            let claimed = with_life(voice_id, now, |l| {
                if l.pending.as_ref() == Some(&pending) {
                    l.pending = None;
                    true
                } else {
                    false
                }
            });
            if claimed && let Err(e) = fire(state, voice_id, &pending, &settings, now).await {
                tracing::warn!(voice_session = %voice_id, "assistant memory: context clear failed: {e}");
            }
        }
    }
}

/// Clear the voice session's context after `pending`'s topics ended:
/// fallback memory for any topic still lacking one, clear, rebuild the
/// prompt with the Recent Memory snapshot, post a `system` event.
pub async fn fire(
    state: &Arc<AppState>,
    voice_id: &str,
    pending: &PendingClear,
    settings: &AssistantSettings,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    for t in &pending.topics {
        super::write_fallback(
            &state.db,
            voice_id,
            settings,
            &t.id,
            &t.title,
            t.summary.as_deref(),
            now,
        )
        .await?;
    }
    crate::routes::sessions::clear_session_core(state, voice_id).await?;
    crate::service::voice_prompt::refresh_memory_snapshot(
        &state.db,
        Some(&state.provider_registry),
        now,
    )
    .await?;
    let kept = if settings.memory_enabled {
        super::list_live(&state.db, voice_id, now).await?.len()
    } else {
        0
    };
    let title = pending
        .topics
        .last()
        .map(|t| t.title.clone())
        .unwrap_or_default();
    let text = format!(
        "Context cleared after \"{title}\" · {kept} {} kept",
        if kept == 1 { "memory" } else { "memories" }
    );
    let event = state
        .db
        .append_event(
            voice_id,
            "system",
            serde_json::json!({
                "text": text,
                "kind": CLEARED_EVENT_KIND,
                "reason": pending.reason,
                "kept": kept,
            }),
        )
        .await?;
    state
        .broadcaster
        .broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "event".into(),
            session_id: voice_id.to_string(),
            data: serde_json::json!({
                "id": event.id,
                "seq": event.seq,
                "ts": event.ts,
                "kind": event.kind,
                "data": serde_json::from_str::<serde_json::Value>(&event.data).unwrap_or_default(),
            }),
        });
    tracing::info!(voice_session = %voice_id, topic = %title, reason = ?pending.reason, kept, "assistant memory: context cleared");
    with_life(voice_id, now, |l| {
        l.last_clear = Some(LastClear {
            at: now,
            reason: pending.reason,
            topic_title: title,
            kept,
        });
    });
    Ok(())
}

/// Evaluate every [`TICK`]; sweep expired memories every [`SWEEP_EVERY`].
/// Called once at boot from `voice_relay::spawn_listener`.
pub fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move {
        // Boot: the agent isn't running yet, so this is the moment to bring
        // the stored snapshot up to date (expired entries out).
        if let Err(e) = crate::service::voice_prompt::refresh_memory_snapshot(
            &state.db,
            Some(&state.provider_registry),
            Utc::now(),
        )
        .await
        {
            tracing::warn!("assistant memory: boot snapshot failed: {e}");
        }
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_sweep: Option<std::time::Instant> = None;
        loop {
            interval.tick().await;
            if last_sweep.is_none_or(|t| t.elapsed() >= SWEEP_EVERY) {
                last_sweep = Some(std::time::Instant::now());
                if let Err(e) = super::sweep(&state.db, Utc::now()).await {
                    tracing::warn!("assistant memory: sweep failed: {e}");
                }
            }
            tick_at(&state, Utc::now()).await;
        }
    });
}

#[cfg(test)]
pub(crate) fn reset_for_test(voice_id: &str) {
    LIVES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(voice_id);
}
