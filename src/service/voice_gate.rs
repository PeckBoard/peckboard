//! Voice relay gate: decides WHEN a relay (another session's question or
//! turn-end update, see [`crate::service::voice_relay`]) reaches the voice
//! assistant session, so the assistant never talks over the user.
//!
//! Every relay is held here first. Nothing is delivered while
//! - the user is speaking (the browser reports interim speech through
//!   `POST /api/voice/activity`), or
//! - a user utterance was sent and the assistant has not finished answering
//!   it (or a relay turn is still running), or
//! - the assistant's reply is still being read aloud,
//!
//! and never before [`QUIET_GAP`] of silence after any of those. Past the
//! gap, relays about the conversation's current *focus* (the session or
//! project the assistant last messaged, read, created, interrupted, or
//! opened with `show_view`, or the relay it is discussing) are delivered,
//! batched into one turn. Everything else is off-topic: it waits until
//! [`IDLE_RELEASE`] passes with no speech from either side, or until the
//! assistant asks for it with the `voice_queue` tool — and then only one
//! topic (source session) at a time, questions first.
//!
//! Durability: every held relay is a row in `voice_relay_queue` (deleted on
//! delivery) and [`spawn_ticker`] reloads the table at boot, so nothing is
//! lost across a restart. Held questions have no timeout: a question stays
//! queued until delivered, and is only dropped when it was answered some
//! other way in the meantime (checked at delivery).

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::db::Db;
use crate::db::models::{Session, VoiceRelayItem};
use crate::service::mcp_server::ExpertDispatcher;
use crate::state::AppState;
use crate::ws::broadcaster::Broadcaster;

/// Silence (no user speech, no assistant turn or speech) required before
/// any relay is delivered.
pub const QUIET_GAP: Duration = Duration::from_millis(1750);
/// Silence after which off-topic relays are released on their own.
pub const IDLE_RELEASE: Duration = Duration::from_secs(45);
/// A "speaking" report holds relays this long unless refreshed (the client
/// re-reports while interim results keep coming).
const SPEAKING_TTL: Duration = Duration::from_secs(6);
/// Caps a lost "tts_end" report.
const TTS_TTL: Duration = Duration::from_secs(90);
/// Caps a lost turn end: an assistant turn stops holding relays after this.
const BUSY_TTL: Duration = Duration::from_secs(300);
/// How often the ticker re-evaluates held relays.
const TICK: Duration = Duration::from_millis(400);
/// Cap on a relay's one-line summary.
const SUMMARY_CHARS: usize = 120;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelayKind {
    Question,
    Update,
}

impl RelayKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RelayKind::Question => "question",
            RelayKind::Update => "update",
        }
    }
}

/// What the browser reports about the conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    /// The user is talking (interim speech that is not our own echo).
    Speaking,
    /// The user stopped without sending anything.
    Idle,
    /// A user utterance was sent: the assistant owes a reply.
    Sent,
    /// The assistant's reply started being read aloud.
    TtsStart,
    /// Nothing more to read aloud.
    TtsEnd,
}

impl Activity {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "speaking" => Activity::Speaking,
            "idle" => Activity::Idle,
            "sent" => Activity::Sent,
            "tts_start" => Activity::TtsStart,
            "tts_end" => Activity::TtsEnd,
            _ => return None,
        })
    }
}

/// One held relay (in-memory mirror of a `voice_relay_queue` row, plus the
/// source session's context for topic matching).
#[derive(Clone, Debug)]
pub struct Held {
    pub id: String,
    pub source_session_id: String,
    pub source_name: String,
    pub source_project_id: Option<String>,
    pub source_parent_id: Option<String>,
    pub kind: RelayKind,
    pub question_event_id: Option<String>,
    pub text: String,
    pub summary: String,
}

/// The voice conversation's current topic.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Focus {
    session_id: Option<String>,
    parent_id: Option<String>,
    project_id: Option<String>,
}

impl Focus {
    fn of_session(s: &Session) -> Self {
        Focus {
            session_id: Some(s.id.clone()),
            parent_id: s.parent_session_id.clone(),
            project_id: s.project_id.clone(),
        }
    }

    fn of_held(h: &Held) -> Self {
        Focus {
            session_id: Some(h.source_session_id.clone()),
            parent_id: h.source_parent_id.clone(),
            project_id: h.source_project_id.clone(),
        }
    }

    /// The focused session itself, its parent, its subagents, or anything
    /// in the focused project.
    fn covers(&self, h: &Held) -> bool {
        if let Some(f) = &self.session_id
            && (h.source_session_id == *f || h.source_parent_id.as_ref() == Some(f))
        {
            return true;
        }
        if self.parent_id.as_ref() == Some(&h.source_session_id) {
            return true;
        }
        matches!((&self.project_id, &h.source_project_id), (Some(a), Some(b)) if a == b)
    }
}

struct Gate {
    queue: Vec<Held>,
    focus: Focus,
    user_active_until: Option<Instant>,
    /// A user utterance or relay turn is being answered since then.
    busy_since: Option<Instant>,
    tts_until: Option<Instant>,
    last_activity: Instant,
    /// Last logged hold reason, so a steady hold logs once, not every tick.
    last_reason: Option<&'static str>,
}

impl Gate {
    fn new(now: Instant) -> Self {
        Gate {
            queue: Vec::new(),
            focus: Focus::default(),
            user_active_until: None,
            busy_since: None,
            tts_until: None,
            // A fresh gate (first use, or after a restart) counts as recent
            // activity: off-topic relays still wait out the idle window.
            last_activity: now,
            last_reason: None,
        }
    }
}

static GATES: LazyLock<Mutex<HashMap<String, Gate>>> = LazyLock::new(Default::default);

fn gates() -> std::sync::MutexGuard<'static, HashMap<String, Gate>> {
    GATES.lock().unwrap_or_else(|e| e.into_inner())
}

fn with_gate<R>(voice_id: &str, now: Instant, f: impl FnOnce(&mut Gate) -> R) -> R {
    let mut g = gates();
    f(g.entry(voice_id.to_string())
        .or_insert_with(|| Gate::new(now)))
}

/// Record a browser activity report for `voice_id`.
pub fn note_activity(voice_id: &str, activity: Activity) {
    note_activity_at(voice_id, activity, Instant::now());
}

pub fn note_activity_at(voice_id: &str, activity: Activity, now: Instant) {
    with_gate(voice_id, now, |g| {
        g.last_activity = now;
        match activity {
            Activity::Speaking => g.user_active_until = Some(now + SPEAKING_TTL),
            Activity::Idle => g.user_active_until = None,
            Activity::Sent => {
                g.user_active_until = None;
                g.busy_since = Some(now);
            }
            Activity::TtsStart => g.tts_until = Some(now + TTS_TTL),
            Activity::TtsEnd => g.tts_until = None,
        }
        if activity != Activity::Speaking && !g.queue.is_empty() {
            tracing::info!(voice_session = %voice_id, ?activity, held = g.queue.len(), "voice gate: activity");
        }
    });
}

/// A session's agent turn ended. Only matters for voice sessions the gate
/// knows (a no-op map lookup otherwise).
pub fn note_turn_end(session_id: &str) {
    note_turn_end_at(session_id, Instant::now());
}

pub fn note_turn_end_at(session_id: &str, now: Instant) {
    if let Some(g) = gates().get_mut(session_id) {
        g.busy_since = None;
        g.last_activity = now;
    }
}

/// The voice assistant turned to `session_id` (messaged, read, created,
/// interrupted, or opened it).
pub async fn focus_session(db: &Db, voice_id: &str, session_id: &str) {
    let Ok(Some(s)) = db.get_session(session_id).await else {
        return;
    };
    let focus = Focus::of_session(&s);
    tracing::info!(voice_session = %voice_id, focus = %s.name, "voice gate: focus -> session");
    with_gate(voice_id, Instant::now(), |g| g.focus = focus);
}

/// The voice assistant opened a project (or a card in it).
pub fn focus_project(voice_id: &str, project_id: &str) {
    tracing::info!(voice_session = %voice_id, project_id, "voice gate: focus -> project");
    with_gate(voice_id, Instant::now(), |g| {
        g.focus = Focus {
            session_id: None,
            parent_id: None,
            project_id: Some(project_id.to_string()),
        }
    });
}

fn summarize(s: &str) -> String {
    let line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > SUMMARY_CHARS {
        let cut: String = line.chars().take(SUMMARY_CHARS).collect();
        format!("{cut}\u{2026}")
    } else {
        line
    }
}

fn held_from(item: &VoiceRelayItem, source: &Session) -> Held {
    Held {
        id: item.id.clone(),
        source_session_id: source.id.clone(),
        source_name: source.name.clone(),
        source_project_id: source.project_id.clone(),
        source_parent_id: source.parent_session_id.clone(),
        kind: if item.kind == "question" {
            RelayKind::Question
        } else {
            RelayKind::Update
        },
        question_event_id: item.question_event_id.clone(),
        text: item.text.clone(),
        summary: item.summary.clone(),
    }
}

/// Hold a relay for `voice_id` (persisted first, so a restart keeps it).
/// `summary_source` is the text the one-line summary is cut from.
pub async fn enqueue(
    db: &Db,
    voice_id: &str,
    source: &Session,
    kind: RelayKind,
    question_event_id: Option<&str>,
    text: &str,
    summary_source: &str,
) {
    if let Some(qid) = question_event_id {
        let dup = gates().get(voice_id).is_some_and(|g| {
            g.queue
                .iter()
                .any(|h| h.question_event_id.as_deref() == Some(qid))
        });
        if dup {
            return;
        }
    }
    let item = VoiceRelayItem {
        id: uuid::Uuid::new_v4().to_string(),
        voice_session_id: voice_id.to_string(),
        source_session_id: source.id.clone(),
        kind: kind.as_str().to_string(),
        question_event_id: question_event_id.map(str::to_string),
        text: text.to_string(),
        summary: summarize(summary_source),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    if let Err(e) = db.insert_voice_relay(item.clone()).await {
        // Still hold it in memory: losing it now would be worse than
        // losing it only on a restart.
        tracing::warn!(voice_session = %voice_id, "voice gate: persisting relay failed: {e}");
    }
    let held = held_from(&item, source);
    tracing::info!(
        voice_session = %voice_id,
        source = %source.name,
        kind = kind.as_str(),
        "voice gate: relay queued"
    );
    with_gate(voice_id, Instant::now(), |g| g.queue.push(held));
}

/// Pending relays for `voice_id`, in queue order.
pub fn list(voice_id: &str) -> Vec<Held> {
    gates()
        .get(voice_id)
        .map(|g| g.queue.clone())
        .unwrap_or_default()
}

/// `(source session, whether it holds a question)` groups in release order:
/// sources with a question first, then by first arrival.
fn topic_order(queue: &[Held]) -> Vec<String> {
    let mut sources: Vec<(String, bool, usize)> = Vec::new();
    for (i, h) in queue.iter().enumerate() {
        match sources.iter_mut().find(|s| s.0 == h.source_session_id) {
            Some(s) => s.1 |= h.kind == RelayKind::Question,
            None => sources.push((
                h.source_session_id.clone(),
                h.kind == RelayKind::Question,
                i,
            )),
        }
    }
    sources.sort_by_key(|s| (!s.1, s.2));
    sources.into_iter().map(|s| s.0).collect()
}

/// Questions first, otherwise queue order.
fn questions_first(mut items: Vec<Held>) -> Vec<Held> {
    items.sort_by_key(|h| h.kind != RelayKind::Question);
    items
}

enum Decision {
    Hold(&'static str),
    /// Deliver these (removed from the queue); `topic` becomes the focus.
    Deliver {
        items: Vec<Held>,
        topic: Option<Focus>,
        why: &'static str,
    },
}

fn decide(g: &mut Gate, now: Instant) -> Decision {
    if g.user_active_until.is_some_and(|t| now < t) {
        return Decision::Hold("user is speaking");
    }
    if g.busy_since.is_some_and(|t| now < t + BUSY_TTL) {
        return Decision::Hold("assistant is answering");
    }
    if g.tts_until.is_some_and(|t| now < t) {
        return Decision::Hold("assistant is speaking");
    }
    let quiet = now.saturating_duration_since(g.last_activity);
    if quiet < QUIET_GAP {
        return Decision::Hold("waiting for a quiet gap");
    }
    let (on, off): (Vec<Held>, Vec<Held>) = g.queue.drain(..).partition(|h| g.focus.covers(h));
    g.queue = off;
    if !on.is_empty() {
        return Decision::Deliver {
            items: questions_first(on),
            topic: None,
            why: "on-topic",
        };
    }
    if quiet < IDLE_RELEASE {
        return Decision::Hold("off-topic: waiting for idle or voice_queue next");
    }
    match take_topic(&mut g.queue) {
        Some((items, topic)) => Decision::Deliver {
            items,
            topic: Some(topic),
            why: "off-topic, released after idle",
        },
        None => Decision::Hold("empty"),
    }
}

/// Remove the next topic group (one source session) from `queue`.
fn take_topic(queue: &mut Vec<Held>) -> Option<(Vec<Held>, Focus)> {
    let source = topic_order(queue).into_iter().next()?;
    let (group, rest): (Vec<Held>, Vec<Held>) =
        queue.drain(..).partition(|h| h.source_session_id == source);
    *queue = rest;
    let topic = Focus::of_held(&group[0]);
    Some((questions_first(group), topic))
}

/// Drop questions answered some other way while held. Deletes their rows.
async fn still_relevant(db: &Db, items: Vec<Held>) -> Vec<Held> {
    let mut out = Vec::with_capacity(items.len());
    let mut stale = Vec::new();
    for h in items {
        if let (RelayKind::Question, Some(qid)) = (h.kind, h.question_event_id.as_deref()) {
            let pending =
                crate::service::questions::pending_question_events(db, &h.source_session_id)
                    .await
                    .map(|p| p.iter().any(|e| e.id == qid))
                    // A failed scan must not lose the question.
                    .unwrap_or(true);
            if !pending {
                tracing::info!(source = %h.source_name, question_id = qid, "voice gate: dropping held question answered elsewhere");
                stale.push(h.id.clone());
                continue;
            }
        }
        out.push(h);
    }
    if let Err(e) = db.delete_voice_relays(stale).await {
        tracing::warn!("voice gate: deleting stale relays failed: {e}");
    }
    out
}

/// Evaluate every voice session's held relays and deliver what is due.
pub async fn tick(db: &Db, broadcaster: &Broadcaster, dispatcher: Option<&dyn ExpertDispatcher>) {
    tick_at(db, broadcaster, dispatcher, Instant::now()).await;
}

pub async fn tick_at(
    db: &Db,
    broadcaster: &Broadcaster,
    dispatcher: Option<&dyn ExpertDispatcher>,
    now: Instant,
) {
    let mut due: Vec<(String, Vec<Held>, &'static str)> = Vec::new();
    {
        let mut all = gates();
        for (voice_id, g) in all.iter_mut() {
            if g.queue.is_empty() {
                g.last_reason = None;
                continue;
            }
            match decide(g, now) {
                Decision::Hold(reason) => {
                    if g.last_reason != Some(reason) {
                        tracing::info!(voice_session = %voice_id, held = g.queue.len(), reason, "voice gate: holding relays");
                        g.last_reason = Some(reason);
                    }
                }
                Decision::Deliver { items, topic, why } => {
                    if let Some(topic) = topic {
                        g.focus = topic;
                    }
                    // The relay turn is an assistant turn: hold the rest
                    // until it ends.
                    g.busy_since = Some(now);
                    g.last_activity = now;
                    g.last_reason = None;
                    due.push((voice_id.clone(), items, why));
                }
            }
        }
    }
    for (voice_id, items, why) in due {
        let items = still_relevant(db, items).await;
        if items.is_empty() {
            // Nothing left to say: don't hold the rest behind a turn that
            // never starts.
            note_turn_end_at(&voice_id, now);
            continue;
        }
        let sources: Vec<&str> = items.iter().map(|h| h.source_name.as_str()).collect();
        tracing::info!(voice_session = %voice_id, count = items.len(), ?sources, why, "voice gate: releasing relays");
        let text = items
            .iter()
            .map(|h| h.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let ids = items.iter().map(|h| h.id.clone()).collect();
        let delivered =
            crate::service::voice_relay::deliver(db, broadcaster, dispatcher, &voice_id, &text)
                .await;
        if !delivered {
            // Keep the rows and put the relays back at the head of the
            // queue: a failed append must not lose a held question.
            with_gate(&voice_id, now, |g| {
                g.busy_since = None;
                let mut back = items;
                back.append(&mut g.queue);
                g.queue = back;
            });
            continue;
        }
        if let Err(e) = db.delete_voice_relays(ids).await {
            tracing::warn!(voice_session = %voice_id, "voice gate: deleting delivered relays failed: {e}");
        }
    }
}

/// `voice_queue next`: take the next topic group now, bypassing the gate
/// (the assistant asked for it, inside its own turn). The group's session
/// becomes the focus. Empty when nothing is queued.
pub async fn take_next(db: &Db, voice_id: &str) -> Vec<Held> {
    let taken = {
        let mut all = gates();
        let Some(g) = all.get_mut(voice_id) else {
            return Vec::new();
        };
        // Anything on the current topic goes first.
        let (on, off): (Vec<Held>, Vec<Held>) = g.queue.drain(..).partition(|h| g.focus.covers(h));
        g.queue = off;
        if !on.is_empty() {
            questions_first(on)
        } else {
            match take_topic(&mut g.queue) {
                Some((items, topic)) => {
                    g.focus = topic;
                    items
                }
                None => Vec::new(),
            }
        }
    };
    if taken.is_empty() {
        return taken;
    }
    let items = still_relevant(db, taken).await;
    let sources: Vec<&str> = items.iter().map(|h| h.source_name.as_str()).collect();
    tracing::info!(voice_session = %voice_id, count = items.len(), ?sources, "voice gate: released by voice_queue next");
    let ids = items.iter().map(|h| h.id.clone()).collect();
    if let Err(e) = db.delete_voice_relays(ids).await {
        tracing::warn!(voice_session = %voice_id, "voice gate: deleting released relays failed: {e}");
    }
    items
}

/// Reload held relays from the DB (boot). Rows whose source session is gone
/// are deleted.
pub async fn load(db: &Db) {
    let rows = match db.list_voice_relays().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("voice gate: loading held relays failed: {e}");
            return;
        }
    };
    let mut orphans = Vec::new();
    let now = Instant::now();
    for row in rows {
        let Ok(Some(source)) = db.get_session(&row.source_session_id).await else {
            orphans.push(row.id);
            continue;
        };
        let held = held_from(&row, &source);
        with_gate(&row.voice_session_id, now, |g| {
            if !g.queue.iter().any(|h| h.id == held.id) {
                g.queue.push(held);
            }
        });
    }
    if !orphans.is_empty() {
        let _ = db.delete_voice_relays(orphans).await;
    }
    let held: usize = gates().values().map(|g| g.queue.len()).sum();
    if held > 0 {
        tracing::info!(held, "voice gate: reloaded held relays");
    }
}

/// Reload the queue, then re-evaluate it every [`TICK`]. Called once from
/// `voice_relay::spawn_listener`.
pub fn spawn_ticker(state: Arc<AppState>) {
    tokio::spawn(async move {
        load(&state.db).await;
        let dispatcher = crate::service::mcp_server::AppExpertDispatcher::new(state.clone());
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            tick(&state.db, &state.broadcaster, Some(&dispatcher)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{NewFolder, NewSession};

    async fn setup() -> (Db, Arc<Broadcaster>) {
        let db = Db::in_memory().unwrap();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "f".into(),
            path: "/tmp".into(),
            created_at: "now".into(),
        })
        .await
        .unwrap();
        (db, Broadcaster::new())
    }

    async fn seed(db: &Db, name: &str, voice: bool) -> Session {
        let now = chrono::Utc::now().to_rfc3339();
        db.create_session(NewSession {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            folder_id: "f1".into(),
            created_at: now.clone(),
            last_activity: now,
            is_expert: voice,
            expert_kind: voice.then(|| "voice".to_string()),
            user_id: Some("u1".into()),
            ..Default::default()
        })
        .await
        .unwrap()
    }

    async fn delivered(db: &Db, voice_id: &str) -> Vec<String> {
        db.list_events_by_session(voice_id, None)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "user")
            .filter_map(|e| {
                serde_json::from_str::<serde_json::Value>(&e.data)
                    .ok()?
                    .get("text")?
                    .as_str()
                    .map(str::to_string)
            })
            .collect()
    }

    async fn update(db: &Db, voice: &Session, source: &Session, text: &str) {
        enqueue(db, &voice.id, source, RelayKind::Update, None, text, text).await;
    }

    #[tokio::test]
    async fn on_topic_waits_for_quiet_gap_and_user_speech() {
        let (db, bc) = setup().await;
        let voice = seed(&db, "Voice", true).await;
        let dev = seed(&db, "dev", false).await;
        focus_session(&db, &voice.id, &dev.id).await;
        let t0 = Instant::now();
        note_activity_at(&voice.id, Activity::Speaking, t0);
        update(&db, &voice, &dev, "[relay] update from dev: done").await;

        // The user is mid-utterance: held.
        tick_at(&db, &bc, None, t0 + Duration::from_secs(2)).await;
        assert!(delivered(&db, &voice.id).await.is_empty());
        // Utterance sent: held while the assistant answers.
        note_activity_at(&voice.id, Activity::Sent, t0 + Duration::from_secs(3));
        tick_at(&db, &bc, None, t0 + Duration::from_secs(10)).await;
        assert!(delivered(&db, &voice.id).await.is_empty());
        // Reply finished, but not yet a quiet gap.
        note_turn_end_at(&voice.id, t0 + Duration::from_secs(11));
        tick_at(&db, &bc, None, t0 + Duration::from_millis(11_500)).await;
        assert!(delivered(&db, &voice.id).await.is_empty());
        // Quiet gap passed: delivered.
        tick_at(&db, &bc, None, t0 + Duration::from_secs(13)).await;
        assert_eq!(
            delivered(&db, &voice.id).await,
            vec!["[relay] update from dev: done"]
        );
        assert!(db.list_voice_relays().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn off_topic_is_held_until_idle_one_topic_at_a_time_questions_first() {
        let (db, bc) = setup().await;
        let voice = seed(&db, "Voice", true).await;
        let a = seed(&db, "alpha", false).await;
        let b = seed(&db, "beta", false).await;
        let focus = seed(&db, "focus", false).await;
        focus_session(&db, &voice.id, &focus.id).await;
        let t0 = Instant::now();
        note_activity_at(&voice.id, Activity::Idle, t0);
        update(&db, &voice, &a, "[relay] update from alpha 1").await;
        update(&db, &voice, &b, "[relay] update from beta").await;
        let q = db
            .append_event(
                &b.id,
                "question",
                serde_json::json!({"questions": [{"question": "Which DB?"}]}),
            )
            .await
            .unwrap();
        enqueue(
            &db,
            &voice.id,
            &b,
            RelayKind::Question,
            Some(&q.id),
            "[relay] question from beta",
            "Which DB?",
        )
        .await;
        update(&db, &voice, &a, "[relay] update from alpha 2").await;

        // Quiet but off-topic: held (and the question never times out).
        tick_at(&db, &bc, None, t0 + Duration::from_secs(30)).await;
        assert!(delivered(&db, &voice.id).await.is_empty());
        assert_eq!(list(&voice.id).len(), 4);

        // Idle long enough: beta's group (it has a question) goes first,
        // batched into one turn, question first.
        tick_at(&db, &bc, None, t0 + Duration::from_secs(46)).await;
        assert_eq!(
            delivered(&db, &voice.id).await,
            vec!["[relay] question from beta\n\n[relay] update from beta"]
        );
        // Alpha stays held behind the relay turn, then waits for idle again.
        let t1 = t0 + Duration::from_secs(50);
        note_turn_end_at(&voice.id, t1);
        tick_at(&db, &bc, None, t1 + Duration::from_secs(5)).await;
        assert_eq!(delivered(&db, &voice.id).await.len(), 1);
        tick_at(&db, &bc, None, t1 + Duration::from_secs(46)).await;
        assert_eq!(
            delivered(&db, &voice.id).await[1],
            "[relay] update from alpha 1\n\n[relay] update from alpha 2"
        );
    }

    #[tokio::test]
    async fn voice_queue_next_releases_the_next_topic_now() {
        let (db, _bc) = setup().await;
        let voice = seed(&db, "Voice", true).await;
        let a = seed(&db, "alpha", false).await;
        let b = seed(&db, "beta", false).await;
        update(&db, &voice, &a, "[relay] alpha").await;
        update(&db, &voice, &b, "[relay] beta").await;

        let first = take_next(&db, &voice.id).await;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].source_name, "alpha");
        // Alpha is now the focus: its next update is on-topic.
        update(&db, &voice, &a, "[relay] alpha again").await;
        let next = take_next(&db, &voice.id).await;
        assert_eq!(next[0].text, "[relay] alpha again");
        assert_eq!(take_next(&db, &voice.id).await[0].source_name, "beta");
        assert!(take_next(&db, &voice.id).await.is_empty());
        assert!(db.list_voice_relays().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn held_relays_survive_a_restart_and_answered_questions_drop() {
        let (db, _bc) = setup().await;
        let voice = seed(&db, "Voice", true).await;
        let a = seed(&db, "alpha", false).await;
        let q = db
            .append_event(
                &a.id,
                "question",
                serde_json::json!({"questions": [{"question": "Which DB?"}]}),
            )
            .await
            .unwrap();
        enqueue(
            &db,
            &voice.id,
            &a,
            RelayKind::Question,
            Some(&q.id),
            "[relay] q",
            "Which DB?",
        )
        .await;
        update(&db, &voice, &a, "[relay] u").await;

        // "Restart": forget the in-memory queue, reload from the DB.
        gates().remove(&voice.id);
        load(&db).await;
        let held = list(&voice.id);
        assert_eq!(held.len(), 2);
        assert_eq!(held[0].summary, "Which DB?");

        // Answered in the UI meanwhile: the question is dropped, not relayed.
        db.append_event(
            &a.id,
            "question-resolved",
            serde_json::json!({"question_id": q.id, "answers": {"0": "x"}}),
        )
        .await
        .unwrap();
        let next = take_next(&db, &voice.id).await;
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].text, "[relay] u");
    }
}
