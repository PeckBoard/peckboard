//! Per-session memory pool (`session_memories`) and the "Memory" prompt
//! section.
//!
//! The pool is the agent's own notebook for the work in progress. A note
//! written through the tools is **short-term**: it carries `expires_at`,
//! fades to a one-line mention once half its life has passed untouched, and
//! is deleted when it expires. Rewriting a note restarts its clock. Notes
//! from before short-term memory existed have no expiry and stay until
//! removed. What should outlive the session's current work belongs in the
//! folder's long-term memory (`folder_memories`).
//!
//! Every accessor is scoped by `session_id` — an id from another session is
//! simply "not found", never a cross-session read or write.

use chrono::{DateTime, SecondsFormat, Utc};
use diesel::connection::Connection;
use diesel::prelude::*;

use super::folder_memories::FOLDER_MEMORY_CATEGORIES;
use crate::db::Db;
use crate::db::models::{FolderMemory, NewSessionMemory, SessionMemory};
use crate::db::schema::session_memories;

/// Longest single entry, in chars. Memories are terse notes, not documents;
/// a cap keeps the rendered prompt section bounded.
pub const MAX_MEMORY_ENTRY_CHARS: usize = 2_000;
/// Most entries one session may hold.
pub const MAX_MEMORY_ENTRIES: usize = 200;
/// Total chars across the whole pool — what the system prompt actually pays
/// for on each spawn.
pub const MAX_MEMORY_TOTAL_CHARS: usize = 24_000;

/// How long a short-term note lives when nothing is configured, in hours.
pub const DEFAULT_SHORT_TERM_HOURS: i64 = 24;
/// Bounds for the configurable short-term lifetime, in hours.
pub const MIN_SHORT_TERM_HOURS: i64 = 1;
pub const MAX_SHORT_TERM_HOURS: i64 = 24 * 90;

/// Long-term notes are shown in full while their total stays under this
/// many chars; past it the prompt lists titles only and the agent reads a
/// note with `memory_read`.
pub const LONG_TERM_PROMPT_BUDGET_CHARS: usize = 12_000;

/// Longest one-line mention of a faded short-term note, in chars.
const FADED_MENTION_CHARS: usize = 120;

/// Fixed-width UTC timestamp for `expires_at`, so plain string comparison
/// in SQL orders the same way time does.
fn expiry_key(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn expiry_after(now: DateTime<Utc>, lifetime_hours: i64) -> String {
    let hours = lifetime_hours.clamp(MIN_SHORT_TERM_HOURS, MAX_SHORT_TERM_HOURS);
    expiry_key(now + chrono::Duration::hours(hours))
}

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

/// The live pool: expired short-term notes are already gone as far as any
/// reader is concerned, whether or not the sweeper has deleted the rows yet.
fn load_pool(conn: &mut SqliteConnection, session_id: &str) -> QueryResult<Vec<SessionMemory>> {
    let now = expiry_key(Utc::now());
    session_memories::table
        .filter(session_memories::session_id.eq(session_id))
        .filter(
            session_memories::expires_at
                .is_null()
                .or(session_memories::expires_at.gt(now)),
        )
        .order((
            session_memories::created_at.asc(),
            session_memories::id.asc(),
        ))
        .select(SessionMemory::as_select())
        .load(conn)
}

fn insert_entry(
    conn: &mut SqliteConnection,
    session_id: &str,
    content: String,
    expires_at: Option<String>,
) -> anyhow::Result<SessionMemory> {
    conn.transaction::<_, anyhow::Error, _>(|conn| {
        let mut pool: Vec<String> = load_pool(conn, session_id)?
            .into_iter()
            .map(|m| m.content)
            .collect();
        pool.push(content.clone());
        check_pool_limits(&pool)?;
        let now = Utc::now().to_rfc3339();
        let new = NewSessionMemory {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: session_id.to_string(),
            content,
            created_at: now.clone(),
            updated_at: now,
            expires_at,
        };
        diesel::insert_into(session_memories::table)
            .values(&new)
            .returning(SessionMemory::as_returning())
            .get_result(conn)
            .map_err(Into::into)
    })
}

impl Db {
    /// Every live memory of `session_id`, oldest first.
    pub async fn list_session_memories(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Vec<SessionMemory>> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| load_pool(conn, &session_id).map_err(Into::into))
            .await
    }

    /// Append one memory that never expires. Validates the entry and the
    /// resulting pool size. The tools write short-term notes
    /// ([`Self::add_short_term_memory`]); this is the shape notes had before
    /// short-term memory existed.
    pub async fn add_session_memory(
        &self,
        session_id: &str,
        content: &str,
    ) -> anyhow::Result<SessionMemory> {
        let session_id = session_id.to_string();
        let content = validate_memory_content(content)?;
        self.with_conn(move |conn| insert_entry(conn, &session_id, content, None))
            .await
    }

    /// Append one short-term memory: deleted `lifetime_hours` after it was
    /// last written.
    pub async fn add_short_term_memory(
        &self,
        session_id: &str,
        content: &str,
        lifetime_hours: i64,
    ) -> anyhow::Result<SessionMemory> {
        let session_id = session_id.to_string();
        let content = validate_memory_content(content)?;
        let expires_at = Some(expiry_after(Utc::now(), lifetime_hours));
        self.with_conn(move |conn| insert_entry(conn, &session_id, content, expires_at))
            .await
    }

    /// Rewrite one memory's content. A short-term note's clock restarts
    /// (`lifetime_hours` from now); a note without expiry stays that way.
    /// `Ok(None)` when `id` is not a live memory of `session_id` — ids from
    /// other sessions are indistinguishable from unknown ones.
    pub async fn update_session_memory(
        &self,
        session_id: &str,
        id: &str,
        content: &str,
        lifetime_hours: i64,
    ) -> anyhow::Result<Option<SessionMemory>> {
        let session_id = session_id.to_string();
        let id = id.to_string();
        let content = validate_memory_content(content)?;
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                let existing = load_pool(conn, &session_id)?;
                let Some(current) = existing.iter().find(|m| m.id == id) else {
                    return Ok(None);
                };
                let expires_at = current
                    .expires_at
                    .as_ref()
                    .map(|_| expiry_after(Utc::now(), lifetime_hours));
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
                let now = Utc::now().to_rfc3339();
                diesel::update(
                    session_memories::table
                        .filter(session_memories::id.eq(&id))
                        .filter(session_memories::session_id.eq(&session_id)),
                )
                .set((
                    session_memories::content.eq(&content),
                    session_memories::updated_at.eq(&now),
                    session_memories::expires_at.eq(&expires_at),
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

    /// Atomically replace the short-term notes of `session_id` with
    /// `entries` (one note per string, in order, each living
    /// `lifetime_hours` from now). Notes without expiry are left alone — a
    /// condense pass must not quietly turn a kept note into one that
    /// expires. All-or-nothing: a validation failure leaves the pool
    /// untouched. Returns the new pool.
    pub async fn compact_session_memories(
        &self,
        session_id: &str,
        entries: &[String],
        lifetime_hours: i64,
    ) -> anyhow::Result<Vec<SessionMemory>> {
        let session_id = session_id.to_string();
        let entries = entries
            .iter()
            .map(|e| validate_memory_content(e))
            .collect::<anyhow::Result<Vec<String>>>()?;
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                let mut pool: Vec<String> = load_pool(conn, &session_id)?
                    .into_iter()
                    .filter(|m| m.expires_at.is_none())
                    .map(|m| m.content)
                    .collect();
                pool.extend(entries.iter().cloned());
                check_pool_limits(&pool)?;
                diesel::delete(
                    session_memories::table
                        .filter(session_memories::session_id.eq(&session_id))
                        .filter(session_memories::expires_at.is_not_null()),
                )
                .execute(conn)?;
                let at = Utc::now();
                let now = at.to_rfc3339();
                let expires_at = Some(expiry_after(at, lifetime_hours));
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
                        expires_at: expires_at.clone(),
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

    /// Delete every short-term note whose time has passed, across all
    /// sessions. Returns how many rows went. Run by the retention sweeper;
    /// readers already skip these rows.
    pub async fn purge_expired_session_memories(&self) -> anyhow::Result<usize> {
        let now = expiry_key(Utc::now());
        self.with_conn(move |conn| {
            diesel::delete(session_memories::table.filter(session_memories::expires_at.le(now)))
                .execute(conn)
                .map_err(Into::into)
        })
        .await
    }
}

/// Whether a short-term note has passed half its life since it was last
/// written — from then on the prompt carries only a one-line mention.
pub fn memory_is_faded(m: &SessionMemory, now: DateTime<Utc>) -> bool {
    let Some(expires) = m
        .expires_at
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
    else {
        return false;
    };
    let Ok(written) = DateTime::parse_from_rfc3339(&m.updated_at) else {
        return false;
    };
    let half = written + (expires - written) / 2;
    now >= half
}

fn time_left(expires_at: &str, now: DateTime<Utc>) -> String {
    let Ok(expires) = DateTime::parse_from_rfc3339(expires_at) else {
        return "soon".into();
    };
    let minutes = (expires.with_timezone(&Utc) - now).num_minutes().max(1);
    if minutes >= 60 {
        format!("{}h", (minutes + 30) / 60)
    } else {
        format!("{minutes}m")
    }
}

/// Push `text` as one bullet body: continuation lines are indented so a
/// multi-line note stays one bullet.
fn push_note_body(out: &mut String, text: &str) {
    let mut first = true;
    for line in text.lines() {
        if !first {
            out.push_str("\n  ");
        }
        out.push_str(line);
        first = false;
    }
}

fn category_heading(category: &str) -> &'static str {
    match category {
        "architecture" => "Architecture",
        "decision" => "Design decisions",
        "structure" => "Code structure",
        "layout" => "Project layout",
        _ => "Conventions",
    }
}

/// The "Memory" system-prompt section: the rules for what to remember where
/// (always present, so every agent on every provider knows them), then the
/// folder's long-term notes and the session's own notes when there are any.
pub fn render_memory_prompt(
    long_term: &[FolderMemory],
    session: &[SessionMemory],
    lifetime_hours: i64,
    now: DateTime<Utc>,
) -> String {
    let hours = lifetime_hours.clamp(MIN_SHORT_TERM_HOURS, MAX_SHORT_TERM_HOURS);
    let mut out = format!(
        "\n# Memory\n\n\
         You keep two kinds of notes with the memory_* tools. Treat what is \
         listed here as established fact unless the user says otherwise. Each \
         entry starts with the id the tools take.\n\n\
         - **Long-term** (this folder, shared by every session working here, \
         never expires): architecture, design decisions and their reasons, \
         code structure, how the project is laid out, and working conventions \
         or user preferences that hold beyond this session. Save with \
         memory_add term=\"long\", a title and a category (architecture, \
         decision, structure, layout, convention). When you learn or decide \
         something of that kind, record it; when it changes, memory_update the \
         note. Don't repeat what the repo's own docs already say.\n\
         - **Short-term** (this session only, deleted {hours} hours after it \
         was last written): what is happening now — the current task and \
         where it stands, open commitments, context needed to carry on after \
         a clear or compaction. Save with memory_add. A note that is still \
         needed: memory_update it to restart its clock. Let finished work \
         expire.\n"
    );

    if !long_term.is_empty() {
        let total: usize = long_term.iter().map(|m| m.content.chars().count()).sum();
        let titles_only = total > LONG_TERM_PROMPT_BUDGET_CHARS;
        out.push_str("\n## Long-Term Memory (Folder)\n");
        if titles_only {
            out.push_str(
                "\nToo long to show in full: titles only. Read a note with \
                 memory_read before relying on it.\n",
            );
        }
        for category in FOLDER_MEMORY_CATEGORIES {
            let notes: Vec<&FolderMemory> = long_term
                .iter()
                .filter(|m| m.category == *category)
                .collect();
            if notes.is_empty() {
                continue;
            }
            out.push_str("\n### ");
            out.push_str(category_heading(category));
            out.push_str("\n\n");
            for m in notes {
                out.push_str("- [");
                out.push_str(&m.id);
                out.push_str("] **");
                out.push_str(&m.title);
                out.push_str("**");
                if !titles_only {
                    out.push_str(" — ");
                    push_note_body(&mut out, &m.content);
                }
                out.push('\n');
            }
        }
    }

    let short: Vec<&SessionMemory> = session.iter().filter(|m| m.expires_at.is_some()).collect();
    if !short.is_empty() {
        out.push_str("\n## Short-Term Memory (Session)\n\n");
        for m in short {
            let left = time_left(m.expires_at.as_deref().unwrap_or_default(), now);
            out.push_str("- [");
            out.push_str(&m.id);
            out.push_str("] ");
            if memory_is_faded(m, now) {
                let first = m.content.lines().next().unwrap_or_default();
                let mention: String = first.chars().take(FADED_MENTION_CHARS).collect();
                out.push_str(&mention);
                if mention.len() < m.content.len() {
                    out.push('…');
                }
                out.push_str(" (fading: gone in ");
                out.push_str(&left);
                out.push_str("; memory_list shows it in full, memory_update keeps it)");
            } else {
                push_note_body(&mut out, &m.content);
                out.push_str(" (expires in ");
                out.push_str(&left);
                out.push(')');
            }
            out.push('\n');
        }
    }

    let kept: Vec<&SessionMemory> = session.iter().filter(|m| m.expires_at.is_none()).collect();
    if !kept.is_empty() {
        out.push_str(
            "\n## Kept Session Notes (No Expiry)\n\n\
             Written before short-term memory existed. They stay until removed. \
             If one belongs in long-term memory, add it there and memory_remove \
             it here.\n\n",
        );
        for m in kept {
            out.push_str("- [");
            out.push_str(&m.id);
            out.push_str("] ");
            push_note_body(&mut out, &m.content);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{NewFolder, NewSession};

    const H: i64 = DEFAULT_SHORT_TERM_HOURS;

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

    /// Rewrite a row's clock so a test can stand at any point of its life.
    async fn set_clock(db: &Db, id: &str, written: DateTime<Utc>, expires: DateTime<Utc>) {
        let id = id.to_string();
        db.with_conn(move |conn| {
            diesel::update(session_memories::table.find(&id))
                .set((
                    session_memories::updated_at.eq(written.to_rfc3339()),
                    session_memories::expires_at.eq(Some(expiry_key(expires))),
                ))
                .execute(conn)
                .map_err(Into::into)
        })
        .await
        .unwrap();
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
            .update_session_memory("s1", &a.id, "user prefers spaces", H)
            .await
            .unwrap()
            .expect("own id updates");
        assert_eq!(updated.content, "user prefers spaces");
        assert!(
            updated.expires_at.is_none(),
            "a note without expiry stays so"
        );

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
            db.update_session_memory("s2", &mine.id, "stolen", H)
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
    async fn short_term_notes_expire_and_a_rewrite_restarts_the_clock() {
        let db = db_with_sessions(&["s1"]).await;
        let kept = db.add_session_memory("s1", "kept forever").await.unwrap();
        let note = db.add_short_term_memory("s1", "doing X", H).await.unwrap();
        assert!(note.expires_at.is_some());

        // Nearly expired, then rewritten: the clock restarts.
        let now = Utc::now();
        set_clock(
            &db,
            &note.id,
            now - chrono::Duration::hours(23),
            now + chrono::Duration::hours(1),
        )
        .await;
        let rewritten = db
            .update_session_memory("s1", &note.id, "still doing X", H)
            .await
            .unwrap()
            .unwrap();
        let expires = DateTime::parse_from_rfc3339(rewritten.expires_at.as_deref().unwrap())
            .unwrap()
            .with_timezone(&Utc);
        assert!(expires > now + chrono::Duration::hours(H - 1));

        // Past its time: gone for readers at once, and for good after the purge.
        set_clock(
            &db,
            &note.id,
            now - chrono::Duration::hours(25),
            now - chrono::Duration::hours(1),
        )
        .await;
        let live = db.list_session_memories("s1").await.unwrap();
        assert_eq!(
            live.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            [kept.id.as_str()]
        );
        assert!(
            db.update_session_memory("s1", &note.id, "too late", H)
                .await
                .unwrap()
                .is_none(),
            "an expired note cannot be revived"
        );
        assert_eq!(db.purge_expired_session_memories().await.unwrap(), 1);
        assert_eq!(db.purge_expired_session_memories().await.unwrap(), 0);
        assert_eq!(db.list_session_memories("s1").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn compact_replaces_short_term_notes_atomically_and_keeps_the_rest() {
        let db = db_with_sessions(&["s1"]).await;
        db.add_session_memory("s1", "kept").await.unwrap();
        db.add_short_term_memory("s1", "one", H).await.unwrap();
        db.add_short_term_memory("s1", "two", H).await.unwrap();

        let pool = db
            .compact_session_memories("s1", &["one+two merged".into(), "three".into()], H)
            .await
            .unwrap();
        assert_eq!(
            pool.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
            ["kept", "one+two merged", "three"]
        );
        assert!(pool[0].expires_at.is_none());
        assert!(pool[1].expires_at.is_some());

        // A blank entry fails validation; the pool must be untouched.
        let err = db
            .compact_session_memories("s1", &["new".into(), "   ".into()], H)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("must not be empty"), "got: {err}");
        let after = db.list_session_memories("s1").await.unwrap();
        assert_eq!(
            after.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
            ["kept", "one+two merged", "three"]
        );

        // Compacting to nothing leaves only the kept note.
        let rest = db.compact_session_memories("s1", &[], H).await.unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].content, "kept");
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
        db.compact_session_memories("s1", &pool, H).await.unwrap();
        let err = db
            .add_short_term_memory("s1", "one more", H)
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

    fn session_note(
        id: &str,
        content: &str,
        written_h_ago: i64,
        left_h: Option<i64>,
    ) -> SessionMemory {
        let now = Utc::now();
        SessionMemory {
            id: id.into(),
            session_id: "s1".into(),
            content: content.into(),
            created_at: String::new(),
            updated_at: (now - chrono::Duration::hours(written_h_ago)).to_rfc3339(),
            expires_at: left_h.map(|h| expiry_key(now + chrono::Duration::hours(h))),
        }
    }

    fn folder_note(id: &str, category: &str, title: &str, content: &str) -> FolderMemory {
        FolderMemory {
            id: id.into(),
            folder_id: "f1".into(),
            category: category.into(),
            title: title.into(),
            content: content.into(),
            source_session_id: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn render_always_carries_the_rules_and_lists_each_kind_of_note() {
        let now = Utc::now();
        let rules_only = render_memory_prompt(&[], &[], 24, now);
        assert!(rules_only.starts_with("\n# Memory\n"));
        assert!(rules_only.contains("deleted 24 hours after"));
        assert!(
            !rules_only.contains("## "),
            "no empty sections: {rules_only}"
        );

        let long = [
            folder_note("c-1", "convention", "Tabs", "indent with tabs"),
            folder_note("a-1", "architecture", "One binary", "axum\nsqlite"),
        ];
        let session = [
            session_note("k-1", "old note", 500, None),
            session_note("f-1", "first line of a long note\nsecond line", 20, Some(4)),
            session_note("n-1", "fresh note", 1, Some(23)),
        ];
        let text = render_memory_prompt(&long, &session, 24, now);
        let arch = text
            .find("- [a-1] **One binary** — axum\n  sqlite\n")
            .unwrap();
        let conv = text.find("- [c-1] **Tabs** — indent with tabs\n").unwrap();
        assert!(arch < conv, "categories list in their fixed order");
        assert!(
            text.contains("- [n-1] fresh note (expires in 23h)\n"),
            "got: {text}"
        );
        assert!(
            text.contains("- [f-1] first line of a long note… (fading: gone in 4h;"),
            "past half its life a note shrinks to one line: {text}"
        );
        assert!(!text.contains("second line"));
        assert!(text.contains("## Kept Session Notes (No Expiry)"));
        assert!(text.contains("- [k-1] old note\n"));
    }

    #[test]
    fn long_term_notes_past_the_budget_are_listed_by_title_only() {
        let big = "z".repeat(MAX_MEMORY_ENTRY_CHARS * 2);
        let long: Vec<FolderMemory> = (0..4)
            .map(|i| {
                folder_note(
                    &format!("a-{i}"),
                    "architecture",
                    &format!("Part {i}"),
                    &big,
                )
            })
            .collect();
        let text = render_memory_prompt(&long, &[], 24, Utc::now());
        assert!(text.contains("titles only"));
        assert!(text.contains("- [a-3] **Part 3**\n"));
        assert!(!text.contains("zzzz"));
    }
}
