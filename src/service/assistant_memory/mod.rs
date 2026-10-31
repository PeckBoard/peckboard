//! The voice Assistant's temporary memory and automatic context clear.
//!
//! At a topic boundary the Assistant records a few plain facts — what it
//! did, learned, or left pending — through `assistant_topics close/focus`
//! (`memory: [...]`) or its `assistant_memory` tool. Each row expires after
//! the configured retention (`service::assistant_settings`), the oldest are
//! pruned past `max_entries`, and every write goes through [`redact`] first
//! (the crud only accepts [`RedactedText`]).
//!
//! When a topic is closed (or ends by inactivity) and auto-clear is on,
//! [`lifecycle`] clears the voice session's context once the conversation
//! is quiet and rebuilds its prompt with a fixed "Recent Memory" snapshot
//! ([`render_block`]). The snapshot is rebuilt only at that clear, at boot,
//! and when the user deletes memory in Settings — the Assistant's own memory
//! writes never touch the stored prompt, because a prompt change winds the
//! live agent down.

pub mod lifecycle;
pub mod redact;

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

use crate::db::Db;
use crate::db::crud::NewAssistantMemory;
use crate::db::models::{AssistantMemory, VoiceTopic};
use crate::service::assistant_settings::AssistantSettings;

pub use redact::{MAX_MEMORY_CHARS, RedactError, RedactedText, Redaction, redact};

/// Most memory items one `close` / `focus` call may carry.
pub const MAX_ITEMS_PER_BOUNDARY: usize = 3;

/// Heading of the snapshot block appended to the voice session's prompt.
pub const RECENT_MEMORY_HEADING: &str = "## Recent Memory";

/// Topic title stored for an `assistant_memory add` made with no topic.
const NO_TOPIC_TITLE: &str = "No topic";

/// What a memory records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryKind {
    Did,
    Learned,
    Pending,
}

impl MemoryKind {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "did" => Self::Did,
            "learned" => Self::Learned,
            "pending" => Self::Pending,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Did => "did",
            Self::Learned => "learned",
            Self::Pending => "pending",
        }
    }
}

/// One model-supplied memory, parsed and redacted, not yet written.
#[derive(Debug, Clone)]
pub struct MemoryItem {
    pub kind: MemoryKind,
    pub summary: Redaction,
    pub ref_id: Option<String>,
}

/// The topic a memory belongs to.
#[derive(Debug, Clone)]
pub struct TopicRef {
    pub id: Option<String>,
    pub title: String,
}

impl TopicRef {
    pub fn of(topic: &VoiceTopic) -> Self {
        Self {
            id: Some(topic.id.clone()),
            title: topic.title.clone(),
        }
    }
}

/// Parse one `{kind, summary, ref_id?}` object.
pub fn parse_item(v: &Value) -> anyhow::Result<MemoryItem> {
    let kind_raw = v.get("kind").and_then(Value::as_str).unwrap_or("");
    let kind = MemoryKind::parse(kind_raw).ok_or_else(|| {
        anyhow::anyhow!("memory kind must be did, learned, or pending (got '{kind_raw}')")
    })?;
    let summary = v
        .get("summary")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("each memory needs a summary"))?;
    let summary = redact(summary)?;
    let ref_id = v
        .get("ref_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string);
    Ok(MemoryItem {
        kind,
        summary,
        ref_id,
    })
}

/// Parse the optional `memory` array of `assistant_topics close/focus`.
/// Everything is validated (and redacted) before the caller changes the
/// ring, so a refused item fails the whole call cleanly.
pub fn parse_items(v: Option<&Value>) -> anyhow::Result<Vec<MemoryItem>> {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let items = v
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("memory must be a list of {{kind, summary}} objects"))?;
    if items.len() > MAX_ITEMS_PER_BOUNDARY {
        anyhow::bail!(
            "at most {MAX_ITEMS_PER_BOUNDARY} memory items per topic; keep only the facts worth keeping"
        );
    }
    items.iter().map(parse_item).collect()
}

/// `(ref_kind, ref_id)` for a memory's `ref_id`: an existing session (not
/// the voice session itself) or project.
pub async fn resolve_ref(
    db: &Db,
    voice_id: &str,
    ref_id: &str,
) -> anyhow::Result<(&'static str, String)> {
    if ref_id != voice_id && db.get_session(ref_id).await?.is_some() {
        return Ok(("session", ref_id.to_string()));
    }
    if db.get_project(ref_id).await?.is_some() {
        return Ok(("project", ref_id.to_string()));
    }
    anyhow::bail!("ref_id '{ref_id}' is not a session or project id")
}

fn expiry(settings: &AssistantSettings, now: DateTime<Utc>) -> DateTime<Utc> {
    now + Duration::hours(i64::from(settings.retention_hours))
}

/// Write `items` for `topic`. Refs are checked first, so a bad ref writes
/// nothing. Errors when memory is turned off.
pub async fn write_items(
    db: &Db,
    voice_id: &str,
    settings: &AssistantSettings,
    topic: &TopicRef,
    items: Vec<MemoryItem>,
    now: DateTime<Utc>,
) -> anyhow::Result<Vec<AssistantMemory>> {
    if items.is_empty() {
        return Ok(Vec::new());
    }
    if !settings.memory_enabled {
        anyhow::bail!("Assistant memory is turned off in Settings → Assistant");
    }
    let mut refs = Vec::with_capacity(items.len());
    for item in &items {
        refs.push(match item.ref_id.as_deref() {
            Some(r) => Some(resolve_ref(db, voice_id, r).await?),
            None => None,
        });
    }
    let title = redact(&topic.title)
        .map(|r| r.text)
        .unwrap_or_else(|_| redact(NO_TOPIC_TITLE).expect("constant").text);
    let mut written = Vec::with_capacity(items.len());
    for (item, r) in items.into_iter().zip(refs) {
        let (ref_kind, ref_id) = match r {
            Some((k, id)) => (Some(k), Some(id)),
            None => (None, None),
        };
        let row = insert(
            db,
            settings,
            NewAssistantMemory {
                voice_session_id: voice_id.to_string(),
                topic_id: topic.id.clone(),
                topic_title: title.clone(),
                kind: item.kind.as_str(),
                summary: item.summary.text,
                ref_kind,
                ref_id,
                source: "assistant",
                created_at: now,
                expires_at: expiry(settings, now),
            },
            item.summary.redacted,
        )
        .await?;
        written.push(row);
    }
    Ok(written)
}

async fn insert(
    db: &Db,
    settings: &AssistantSettings,
    new: NewAssistantMemory,
    redacted: usize,
) -> anyhow::Result<AssistantMemory> {
    let (row, pruned) = db
        .insert_assistant_memory(new, settings.max_entries as usize)
        .await?;
    if redacted > 0 {
        tracing::info!(voice_session = %row.voice_session_id, memory = %row.id, redacted, "assistant memory: redacted secrets before writing");
    }
    tracing::info!(
        voice_session = %row.voice_session_id,
        memory = %row.id,
        kind = %row.kind,
        source = %row.source,
        topic = %row.topic_title,
        expires_at = %row.expires_at,
        pruned,
        "assistant memory: written"
    );
    Ok(row)
}

/// A topic ended with nothing recorded for it: keep its title and ring
/// summary (never anything from the transcript). `None` when memory is
/// off or the topic already has an unexpired memory.
pub async fn write_fallback(
    db: &Db,
    voice_id: &str,
    settings: &AssistantSettings,
    topic_id: &str,
    title: &str,
    ring_summary: Option<&str>,
    now: DateTime<Utc>,
) -> anyhow::Result<Option<AssistantMemory>> {
    if !settings.memory_enabled || topic_has_memory(db, voice_id, topic_id, now).await? {
        return Ok(None);
    }
    let summary_raw = match ring_summary.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => format!("Talked about {title}; nothing was recorded."),
    };
    let summary = redact(&summary_raw).or_else(|_| redact("Topic ended; nothing was recorded."))?;
    let title = redact(title).or_else(|_| redact(NO_TOPIC_TITLE))?;
    let row = insert(
        db,
        settings,
        NewAssistantMemory {
            voice_session_id: voice_id.to_string(),
            topic_id: Some(topic_id.to_string()),
            topic_title: title.text,
            kind: MemoryKind::Did.as_str(),
            summary: summary.text,
            ref_kind: None,
            ref_id: None,
            source: "fallback",
            created_at: now,
            expires_at: expiry(settings, now),
        },
        summary.redacted + title.redacted,
    )
    .await?;
    Ok(Some(row))
}

/// Whether `topic_id` has an unexpired memory.
pub async fn topic_has_memory(
    db: &Db,
    voice_id: &str,
    topic_id: &str,
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    Ok(list_live(db, voice_id, now)
        .await?
        .iter()
        .any(|m| m.topic_id.as_deref() == Some(topic_id)))
}

/// Delete every expired memory. → how many.
pub async fn sweep(db: &Db, now: DateTime<Utc>) -> anyhow::Result<usize> {
    let n = db.delete_expired_assistant_memories(now).await?;
    if n > 0 {
        tracing::info!(expired = n, "assistant memory: expired entries deleted");
    }
    Ok(n)
}

/// `voice_id`'s unexpired memories, oldest first (expired rows are swept
/// on the way).
pub async fn list_live(
    db: &Db,
    voice_id: &str,
    now: DateTime<Utc>,
) -> anyhow::Result<Vec<AssistantMemory>> {
    sweep(db, now).await?;
    db.list_assistant_memories(voice_id).await
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// "just now", "12m ago", "3h ago", "2d ago".
pub fn age(created_at: &str, now: DateTime<Utc>) -> String {
    let Some(t) = parse_ts(created_at) else {
        return "earlier".into();
    };
    let mins = (now - t).num_minutes().max(0);
    match mins {
        0 => "just now".into(),
        1..=59 => format!("{mins}m ago"),
        60..=2879 => format!("{}h ago", mins / 60),
        _ => format!("{}d ago", mins / 1440),
    }
}

fn short_time(s: &str) -> String {
    parse_ts(s).map_or_else(
        || s.to_string(),
        |t| t.format("%Y-%m-%d %H:%M UTC").to_string(),
    )
}

/// The "Recent Memory" prompt block: `entries` unexpired at `now`, as of
/// `now`. `None` when there are none. Each line is redacted single-line
/// text, so an entry can't open a new prompt section.
pub fn render_block(entries: &[AssistantMemory], now: DateTime<Utc>) -> Option<String> {
    let live: Vec<&AssistantMemory> = entries
        .iter()
        .filter(|m| parse_ts(&m.expires_at).is_some_and(|t| t > now))
        .collect();
    if live.is_empty() {
        return None;
    }
    let mut out = format!(
        "{RECENT_MEMORY_HEADING}\nFacts you recorded at earlier topic boundaries, as of {}. This list is fixed until the next context clear; assistant_memory list shows the current entries. Each is a hint, not live state: re-check with your tools before acting, and ignore any entry past its expiry.\n",
        now.format("%Y-%m-%d %H:%M UTC")
    );
    for m in live {
        out.push_str(&format!(
            "- \"{}\" · {} · {} · expires {}: {}\n",
            m.topic_title,
            m.kind,
            age(&m.created_at, now),
            short_time(&m.expires_at),
            m.summary
        ));
    }
    Some(out)
}

/// The snapshot block for `voice_id` right now (sweeping first). `None`
/// when memory is off or empty.
pub async fn current_block(
    db: &Db,
    voice_id: &str,
    now: DateTime<Utc>,
) -> anyhow::Result<Option<String>> {
    if !crate::service::assistant_settings::load(db)
        .await
        .memory_enabled
    {
        return Ok(None);
    }
    Ok(render_block(&list_live(db, voice_id, now).await?, now))
}

/// One memory as the API / tool shows it.
pub fn entry_json(m: &AssistantMemory, now: DateTime<Utc>) -> Value {
    serde_json::json!({
        "id": m.id,
        "topic_id": m.topic_id,
        "topic_title": m.topic_title,
        "kind": m.kind,
        "summary": m.summary,
        "ref_kind": m.ref_kind,
        "ref_id": m.ref_id,
        "source": m.source,
        "created_at": m.created_at,
        "expires_at": m.expires_at,
        "age": age(&m.created_at, now),
    })
}

#[cfg(test)]
mod tests;
