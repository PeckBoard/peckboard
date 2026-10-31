//! The voice Assistant's temporary memory (`assistant_memories`), written
//! and read through `service::assistant_memory`.
//!
//! Every accessor is scoped by the voice session id. Timestamps are stored
//! as fixed-width RFC 3339 UTC strings ([`memory_ts`]), so expiry is a plain
//! string comparison in SQL.

use chrono::{DateTime, Utc};
use diesel::connection::Connection;
use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::AssistantMemory;
use crate::db::schema::assistant_memories;
use crate::service::assistant_memory::RedactedText;

/// The stored timestamp form: `2026-10-31T12:00:00.000Z`. Fixed width, so
/// lexical order is time order.
pub fn memory_ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// One memory to write. The text fields are [`RedactedText`] — only
/// `service::assistant_memory::redact` makes those — so nothing reaches the
/// table without going through redaction.
pub struct NewAssistantMemory {
    pub voice_session_id: String,
    pub topic_id: Option<String>,
    pub topic_title: RedactedText,
    /// `did` | `learned` | `pending`.
    pub kind: &'static str,
    pub summary: RedactedText,
    /// `session` | `project`, with `ref_id`.
    pub ref_kind: Option<&'static str>,
    pub ref_id: Option<String>,
    /// `assistant` | `fallback`.
    pub source: &'static str,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

fn load_all(conn: &mut SqliteConnection, voice_id: &str) -> QueryResult<Vec<AssistantMemory>> {
    assistant_memories::table
        .filter(assistant_memories::voice_session_id.eq(voice_id))
        .order((
            assistant_memories::created_at.asc(),
            assistant_memories::id.asc(),
        ))
        .select(AssistantMemory::as_select())
        .load(conn)
}

impl Db {
    /// Insert one memory, then prune the voice session's oldest rows beyond
    /// `max_entries`, in one transaction. → the row and how many were pruned.
    pub async fn insert_assistant_memory(
        &self,
        new: NewAssistantMemory,
        max_entries: usize,
    ) -> anyhow::Result<(AssistantMemory, usize)> {
        let row = AssistantMemory {
            id: uuid::Uuid::new_v4().to_string(),
            voice_session_id: new.voice_session_id,
            topic_id: new.topic_id,
            topic_title: new.topic_title.into_inner(),
            kind: new.kind.to_string(),
            summary: new.summary.into_inner(),
            ref_kind: new.ref_kind.map(str::to_string),
            ref_id: new.ref_id,
            source: new.source.to_string(),
            created_at: memory_ts(new.created_at),
            expires_at: memory_ts(new.expires_at),
        };
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                diesel::insert_into(assistant_memories::table)
                    .values(&row)
                    .execute(conn)?;
                let all = load_all(conn, &row.voice_session_id)?;
                let excess: Vec<String> = all
                    .iter()
                    .take(all.len().saturating_sub(max_entries.max(1)))
                    .map(|m| m.id.clone())
                    .collect();
                let pruned = if excess.is_empty() {
                    0
                } else {
                    diesel::delete(
                        assistant_memories::table.filter(assistant_memories::id.eq_any(&excess)),
                    )
                    .execute(conn)?
                };
                Ok((row, pruned))
            })
        })
        .await
    }

    /// `voice_id`'s memories, oldest first (expired ones included — sweep
    /// first; see `service::assistant_memory::list_live`).
    pub async fn list_assistant_memories(
        &self,
        voice_id: &str,
    ) -> anyhow::Result<Vec<AssistantMemory>> {
        let voice_id = voice_id.to_string();
        self.with_conn(move |conn| load_all(conn, &voice_id).map_err(Into::into))
            .await
    }

    /// Delete every memory (any voice session) that expired at or before
    /// `now`. → how many.
    pub async fn delete_expired_assistant_memories(
        &self,
        now: DateTime<Utc>,
    ) -> anyhow::Result<usize> {
        let now = memory_ts(now);
        self.with_conn(move |conn| {
            diesel::delete(
                assistant_memories::table.filter(assistant_memories::expires_at.le(&now)),
            )
            .execute(conn)
            .map_err(Into::into)
        })
        .await
    }

    /// Delete one of `voice_id`'s memories. → whether it existed.
    pub async fn delete_assistant_memory(&self, voice_id: &str, id: &str) -> anyhow::Result<bool> {
        let (voice_id, id) = (voice_id.to_string(), id.to_string());
        self.with_conn(move |conn| {
            let n = diesel::delete(
                assistant_memories::table
                    .filter(assistant_memories::voice_session_id.eq(&voice_id))
                    .filter(assistant_memories::id.eq(&id)),
            )
            .execute(conn)?;
            Ok(n > 0)
        })
        .await
    }

    /// Delete all of `voice_id`'s memories. → how many.
    pub async fn delete_all_assistant_memories(&self, voice_id: &str) -> anyhow::Result<usize> {
        let voice_id = voice_id.to_string();
        self.with_conn(move |conn| {
            diesel::delete(
                assistant_memories::table
                    .filter(assistant_memories::voice_session_id.eq(&voice_id)),
            )
            .execute(conn)
            .map_err(Into::into)
        })
        .await
    }
}
