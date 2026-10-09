//! Per-session durable memory pool (`session_memories`).
//!
//! The pool is the agent's own notebook: facts, decisions and user
//! preferences it wants to keep across `clear_session` and context
//! compaction. Every accessor is scoped by `session_id` — an id from another
//! session is simply "not found", never a cross-session read or write.

use diesel::connection::Connection;
use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::{NewSessionMemory, SessionMemory};
use crate::db::schema::session_memories;

/// Longest single entry, in chars. Memories are terse notes, not documents;
/// a cap keeps the rendered prompt section bounded.
pub const MAX_MEMORY_ENTRY_CHARS: usize = 2_000;
/// Most entries one session may hold.
pub const MAX_MEMORY_ENTRIES: usize = 200;
/// Total chars across the whole pool — what the system prompt actually pays
/// for on each spawn.
pub const MAX_MEMORY_TOTAL_CHARS: usize = 24_000;

/// Validate one entry's content: trimmed, non-empty, within the per-entry cap.
pub fn validate_memory_content(raw: &str) -> anyhow::Result<String> {
    let content = raw.trim();
    if content.is_empty() {
        anyhow::bail!("memory content must not be empty");
    }
    let len = content.chars().count();
    if len > MAX_MEMORY_ENTRY_CHARS {
        anyhow::bail!(
            "memory entry is {len} chars; the limit is {MAX_MEMORY_ENTRY_CHARS}. \
             Keep each memory a short, self-contained note."
        );
    }
    Ok(content.to_string())
}

/// Validate a whole pool (counts + total size), after each entry passed
/// [`validate_memory_content`].
fn check_pool_limits(entries: &[String]) -> anyhow::Result<()> {
    if entries.len() > MAX_MEMORY_ENTRIES {
        anyhow::bail!(
            "the memory pool would hold {} entries; the limit is {MAX_MEMORY_ENTRIES}. \
             Call memory_compact to condense it.",
            entries.len()
        );
    }
    let total: usize = entries.iter().map(|e| e.chars().count()).sum();
    if total > MAX_MEMORY_TOTAL_CHARS {
        anyhow::bail!(
            "the memory pool would total {total} chars; the limit is {MAX_MEMORY_TOTAL_CHARS}. \
             Call memory_compact to condense it."
        );
    }
    Ok(())
}

fn load_pool(conn: &mut SqliteConnection, session_id: &str) -> QueryResult<Vec<SessionMemory>> {
    session_memories::table
        .filter(session_memories::session_id.eq(session_id))
        .order((
            session_memories::created_at.asc(),
            session_memories::id.asc(),
        ))
        .select(SessionMemory::as_select())
        .load(conn)
}

impl Db {
    /// Every memory of `session_id`, oldest first.
    pub async fn list_session_memories(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Vec<SessionMemory>> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| load_pool(conn, &session_id).map_err(Into::into))
            .await
    }

    /// Append one memory. Validates the entry and the resulting pool size.
    pub async fn add_session_memory(
        &self,
        session_id: &str,
        content: &str,
    ) -> anyhow::Result<SessionMemory> {
        let session_id = session_id.to_string();
        let content = validate_memory_content(content)?;
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                let mut pool: Vec<String> = load_pool(conn, &session_id)?
                    .into_iter()
                    .map(|m| m.content)
                    .collect();
                pool.push(content.clone());
                check_pool_limits(&pool)?;
                let now = chrono::Utc::now().to_rfc3339();
                let new = NewSessionMemory {
                    id: uuid::Uuid::new_v4().to_string(),
                    session_id: session_id.clone(),
                    content,
                    created_at: now.clone(),
                    updated_at: now,
                };
                diesel::insert_into(session_memories::table)
                    .values(&new)
                    .returning(SessionMemory::as_returning())
                    .get_result(conn)
                    .map_err(Into::into)
            })
        })
        .await
    }

    /// Rewrite one memory's content. `Ok(None)` when `id` is not a memory of
    /// `session_id` — ids from other sessions are indistinguishable from
    /// unknown ones.
    pub async fn update_session_memory(
        &self,
        session_id: &str,
        id: &str,
        content: &str,
    ) -> anyhow::Result<Option<SessionMemory>> {
        let session_id = session_id.to_string();
        let id = id.to_string();
        let content = validate_memory_content(content)?;
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                let existing = load_pool(conn, &session_id)?;
                if !existing.iter().any(|m| m.id == id) {
                    return Ok(None);
                }
                let pool: Vec<String> = existing
                    .into_iter()
                    .map(|m| {
                        if m.id == id {
                            content.clone()
                        } else {
                            m.content
                        }
                    })
                    .collect();
                check_pool_limits(&pool)?;
                let now = chrono::Utc::now().to_rfc3339();
                diesel::update(
                    session_memories::table
                        .filter(session_memories::id.eq(&id))
                        .filter(session_memories::session_id.eq(&session_id)),
                )
                .set((
                    session_memories::content.eq(&content),
                    session_memories::updated_at.eq(&now),
                ))
                .execute(conn)?;
                session_memories::table
                    .find(&id)
                    .select(SessionMemory::as_select())
                    .first(conn)
                    .optional()
                    .map_err(Into::into)
            })
        })
        .await
    }

    /// Delete the given memories of `session_id`. Returns how many rows went;
    /// ids that are unknown or belong to another session are skipped.
    pub async fn remove_session_memories(
        &self,
        session_id: &str,
        ids: &[String],
    ) -> anyhow::Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let session_id = session_id.to_string();
        let ids = ids.to_vec();
        self.with_conn(move |conn| {
            diesel::delete(
                session_memories::table
                    .filter(session_memories::session_id.eq(&session_id))
                    .filter(session_memories::id.eq_any(&ids)),
            )
            .execute(conn)
            .map_err(Into::into)
        })
        .await
    }

    /// Atomically replace the whole pool of `session_id` with `entries`
    /// (one memory per string, in order). All-or-nothing: a validation
    /// failure leaves the existing pool untouched. Returns the new pool.
    pub async fn compact_session_memories(
        &self,
        session_id: &str,
        entries: &[String],
    ) -> anyhow::Result<Vec<SessionMemory>> {
        let session_id = session_id.to_string();
        let entries = entries
            .iter()
            .map(|e| validate_memory_content(e))
            .collect::<anyhow::Result<Vec<String>>>()?;
        check_pool_limits(&entries)?;
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                diesel::delete(
                    session_memories::table.filter(session_memories::session_id.eq(&session_id)),
                )
                .execute(conn)?;
                let now = chrono::Utc::now().to_rfc3339();
                // Sequential ids keep the insertion order stable even though
                // every row shares one `created_at`.
                let rows: Vec<NewSessionMemory> = entries
                    .iter()
                    .enumerate()
                    .map(|(idx, content)| NewSessionMemory {
                        id: format!("{idx:04}-{}", uuid::Uuid::new_v4()),
                        session_id: session_id.clone(),
                        content: content.clone(),
                        created_at: now.clone(),
                        updated_at: now.clone(),
                    })
                    .collect();
                if !rows.is_empty() {
                    diesel::insert_into(session_memories::table)
                        .values(&rows)
                        .execute(conn)?;
                }
                load_pool(conn, &session_id).map_err(Into::into)
            })
        })
        .await
    }
}

/// The "Session memory" system-prompt section for a pool, or `None` when the
/// pool is empty (so an empty pool costs no prompt tokens at all).
pub fn render_memory_prompt(entries: &[SessionMemory]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let mut out = String::from(
        "\n# Session memory\n\n\
         Your durable notes for this session, kept across clears and context \
         compaction. Treat them as established facts unless the user says \
         otherwise. Manage them with the memory_* tools (memory_add, \
         memory_update, memory_remove, memory_compact); each line starts with \
         the entry id those tools take.\n\n",
    );
    for m in entries {
        out.push_str("- [");
        out.push_str(&m.id);
        out.push_str("] ");
        // Keep one entry per bullet even if the note has line breaks.
        let mut first = true;
        for line in m.content.lines() {
            if !first {
                out.push_str("\n  ");
            }
            out.push_str(line);
            first = false;
        }
        out.push('\n');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{NewFolder, NewSession};

    async fn db_with_sessions(ids: &[&str]) -> Db {
        let db = Db::in_memory().unwrap();
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        for id in ids {
            db.create_session(NewSession {
                id: id.to_string(),
                name: id.to_string(),
                folder_id: "f1".into(),
                created_at: ts.clone(),
                last_activity: ts.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        db
    }

    #[tokio::test]
    async fn add_list_update_remove_round_trip() {
        let db = db_with_sessions(&["s1"]).await;
        let a = db
            .add_session_memory("s1", "  user prefers tabs ")
            .await
            .unwrap();
        assert_eq!(a.content, "user prefers tabs", "content is trimmed");
        let b = db
            .add_session_memory("s1", "deploy target is prod-eu")
            .await
            .unwrap();

        let listed = db.list_session_memories("s1").await.unwrap();
        assert_eq!(
            listed.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            [a.id.as_str(), b.id.as_str()]
        );

        let updated = db
            .update_session_memory("s1", &a.id, "user prefers spaces")
            .await
            .unwrap()
            .expect("own id updates");
        assert_eq!(updated.content, "user prefers spaces");

        assert_eq!(
            db.remove_session_memories("s1", std::slice::from_ref(&a.id))
                .await
                .unwrap(),
            1
        );
        let left = db.list_session_memories("s1").await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, b.id);
    }

    #[tokio::test]
    async fn pools_are_isolated_per_session() {
        let db = db_with_sessions(&["s1", "s2"]).await;
        let mine = db.add_session_memory("s1", "mine").await.unwrap();
        db.add_session_memory("s2", "theirs").await.unwrap();

        assert_eq!(db.list_session_memories("s2").await.unwrap().len(), 1);
        // s2 can neither update nor remove s1's entry.
        assert!(
            db.update_session_memory("s2", &mine.id, "stolen")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.remove_session_memories("s2", std::slice::from_ref(&mine.id))
                .await
                .unwrap(),
            0
        );
        let s1 = db.list_session_memories("s1").await.unwrap();
        assert_eq!(s1[0].content, "mine");
    }

    #[tokio::test]
    async fn compact_replaces_atomically_and_rolls_back_on_bad_input() {
        let db = db_with_sessions(&["s1"]).await;
        db.add_session_memory("s1", "one").await.unwrap();
        db.add_session_memory("s1", "two").await.unwrap();

        let pool = db
            .compact_session_memories("s1", &["one+two merged".into(), "three".into()])
            .await
            .unwrap();
        assert_eq!(
            pool.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
            ["one+two merged", "three"]
        );

        // A blank entry fails validation; the pool must be untouched.
        let err = db
            .compact_session_memories("s1", &["kept".into(), "   ".into()])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("must not be empty"), "got: {err}");
        let after = db.list_session_memories("s1").await.unwrap();
        assert_eq!(
            after.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
            ["one+two merged", "three"]
        );

        // Compacting to nothing empties the pool.
        assert!(
            db.compact_session_memories("s1", &[])
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn size_caps_are_enforced_with_clear_errors() {
        let db = db_with_sessions(&["s1"]).await;
        let huge = "x".repeat(MAX_MEMORY_ENTRY_CHARS + 1);
        let err = db
            .add_session_memory("s1", &huge)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("limit is"), "got: {err}");

        let fill = "y".repeat(MAX_MEMORY_ENTRY_CHARS);
        let n = MAX_MEMORY_TOTAL_CHARS / MAX_MEMORY_ENTRY_CHARS;
        let pool: Vec<String> = (0..n).map(|_| fill.clone()).collect();
        db.compact_session_memories("s1", &pool).await.unwrap();
        let err = db
            .add_session_memory("s1", "one more")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("memory_compact"), "got: {err}");
    }

    #[tokio::test]
    async fn memories_survive_event_wipe_and_die_with_the_session() {
        let db = db_with_sessions(&["s1"]).await;
        db.append_event("s1", "user", serde_json::json!({ "text": "hi" }))
            .await
            .unwrap();
        db.add_session_memory("s1", "remember me").await.unwrap();

        // What clear_session_core does to the transcript.
        db.delete_events_by_session("s1").await.unwrap();
        db.replace_session_todos("s1", crate::todo::TodoSnapshot::default())
            .await
            .unwrap();
        assert_eq!(db.list_session_memories("s1").await.unwrap().len(), 1);

        assert!(db.delete_session("s1").await.unwrap());
        assert!(db.list_session_memories("s1").await.unwrap().is_empty());
    }

    #[test]
    fn render_is_empty_for_an_empty_pool_and_lists_ids_otherwise() {
        assert!(render_memory_prompt(&[]).is_none());
        let m = SessionMemory {
            id: "m-1".into(),
            session_id: "s1".into(),
            content: "line one\nline two".into(),
            created_at: String::new(),
            updated_at: String::new(),
        };
        let text = render_memory_prompt(&[m]).unwrap();
        assert!(text.starts_with("\n# Session memory\n"));
        assert!(
            text.contains("- [m-1] line one\n  line two\n"),
            "got: {text}"
        );
    }
}
