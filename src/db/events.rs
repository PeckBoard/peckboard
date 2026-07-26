use diesel::prelude::*;
use diesel::result::{DatabaseErrorKind, Error as DieselError};
use diesel::sql_types::{BigInt, Text};
use diesel::sqlite::SqliteConnection;
use std::time::{SystemTime, UNIX_EPOCH};

use super::Db;
use super::models::Event;
use super::schema::events;

/// How many times [`insert_event_next_seq`] re-derives a `seq` before giving
/// up. Each retry costs one failed INSERT; a collision needs a second writer
/// on the same DB file, so more than a couple of rounds is pathological.
const APPEND_SEQ_MAX_ATTEMPTS: u32 = 16;

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Insert one event row, letting SQLite assign `seq` *inside* the INSERT.
///
/// The old shape was read-then-insert: `SELECT MAX(seq)` in one statement,
/// `INSERT` in the next. Two appenders interleaving there both compute the
/// same next seq, `UNIQUE(session_id, seq)` fires for the loser, and the
/// caller's only option is to drop the event. Here `INSERT ... SELECT
/// COALESCE((SELECT MAX(seq) ...), 0) + 1` derives the number in the same
/// statement, and the unique index stays as the backstop: on violation we
/// retry with a freshly computed seq instead of losing the event.
///
/// Returns the inserted row (read back by its uuid, so the read-back cannot
/// pick up a different writer's row).
fn insert_event_next_seq(
    conn: &mut SqliteConnection,
    session_id: &str,
    kind: &str,
    data: &str,
) -> anyhow::Result<Event> {
    for attempt in 1..=APPEND_SEQ_MAX_ATTEMPTS {
        let id = uuid::Uuid::new_v4().to_string();
        let ts = now_millis();

        let inserted = diesel::sql_query(
            "INSERT INTO events (id, session_id, seq, ts, kind, data) \
             SELECT ?, ?, COALESCE((SELECT MAX(seq) FROM events WHERE session_id = ?), 0) + 1, ?, ?, ?",
        )
        .bind::<Text, _>(&id)
        .bind::<Text, _>(session_id)
        .bind::<Text, _>(session_id)
        .bind::<BigInt, _>(ts)
        .bind::<Text, _>(kind)
        .bind::<Text, _>(data)
        .execute(conn);

        match inserted {
            Ok(_) => {
                return events::table
                    .filter(events::id.eq(&id))
                    .select(Event::as_select())
                    .first(conn)
                    .map_err(Into::into);
            }
            Err(DieselError::DatabaseError(DatabaseErrorKind::UniqueViolation, _)) => {
                tracing::debug!(
                    session_id = session_id,
                    attempt = attempt,
                    "event seq collision, retrying with a fresh seq"
                );
            }
            Err(e) => return Err(e.into()),
        }
    }

    anyhow::bail!(
        "failed to assign an event seq for session {session_id} after {APPEND_SEQ_MAX_ATTEMPTS} attempts"
    )
}

impl Db {
    /// Append an event with automatic seq assignment and server-stamped timestamp.
    /// Returns the appended event.
    pub async fn append_event(
        &self,
        session_id: &str,
        kind: &str,
        data: serde_json::Value,
    ) -> anyhow::Result<Event> {
        let session_id = session_id.to_string();
        let kind = kind.to_string();
        let data_str = serde_json::to_string(&data)?;

        self.with_conn(move |conn| insert_event_next_seq(conn, &session_id, &kind, &data_str))
            .await
    }

    /// Synchronous twin of [`append_event`], for WASM plugin host
    /// functions that run inside a blocking extism call. Inserts ONE event
    /// row with the same seq/id/ts scheme as the async path — `seq` is the
    /// per-session `max(seq) + 1` (or 1 for the first), assigned atomically
    /// by [`insert_event_next_seq`], `id` a fresh uuid, `ts` millis since the
    /// Unix epoch. `data` is stored verbatim (already JSON-encoded by the
    /// caller). Does NOT broadcast.
    pub(crate) fn append_event_blocking(
        &self,
        session_id: &str,
        kind: &str,
        data: &str,
    ) -> anyhow::Result<()> {
        self.with_conn_blocking(|conn| {
            insert_event_next_seq(conn, session_id, kind, data).map(|_| ())
        })
    }

    /// Synchronous read of a session's events in `seq` order, for WASM plugin
    /// host functions running inside a blocking extism call (e.g. resolving a
    /// plugin-emitted question's answer). Mirrors the async listing's ordering.
    pub(crate) fn list_events_by_session_blocking(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Vec<Event>> {
        let session_id = session_id.to_string();
        self.with_conn_blocking(move |conn| {
            events::table
                .filter(events::session_id.eq(&session_id))
                .select(Event::as_select())
                .order(events::seq.asc())
                .load(conn)
                .map_err(Into::into)
        })
    }

    /// Synchronous twin of [`events_since`], for WASM plugin host functions
    /// running inside a blocking extism call. Returns the session's events
    /// with `seq` greater than `since_seq`, ordered by `seq` ascending, capped
    /// at `limit` rows.
    pub(crate) fn events_since_blocking(
        &self,
        session_id: &str,
        since_seq: i32,
        limit: i64,
    ) -> anyhow::Result<Vec<Event>> {
        let session_id = session_id.to_string();
        self.with_conn_blocking(move |conn| {
            events::table
                .filter(events::session_id.eq(&session_id))
                .filter(events::seq.gt(since_seq))
                .select(Event::as_select())
                .order(events::seq.asc())
                .limit(limit)
                .load(conn)
                .map_err(Into::into)
        })
    }

    /// Get events since a specific seq number (exclusive).
    pub async fn events_since(
        &self,
        session_id: &str,
        since_seq: i32,
    ) -> anyhow::Result<Vec<Event>> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            events::table
                .filter(events::session_id.eq(&session_id))
                .filter(events::seq.gt(since_seq))
                .select(Event::as_select())
                .order(events::seq.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }
    /// One page of a session's events with `seq` greater than `since_seq`,
    /// ordered by `seq` ascending, at most `limit` rows. Async twin of
    /// [`events_since_blocking`]; the WS resume path pages through a long
    /// backlog with it instead of loading (and then truncating) the whole
    /// tail in one query.
    pub async fn events_since_page(
        &self,
        session_id: &str,
        since_seq: i32,
        limit: i64,
    ) -> anyhow::Result<Vec<Event>> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            events::table
                .filter(events::session_id.eq(&session_id))
                .filter(events::seq.gt(since_seq))
                .select(Event::as_select())
                .order(events::seq.asc())
                .limit(limit)
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Get the last N events for a session (tail query).
    pub async fn events_tail(&self, session_id: &str, limit: i64) -> anyhow::Result<Vec<Event>> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            // Get the last N events by ordering desc and limiting, then reverse.
            let mut events_vec: Vec<Event> = events::table
                .filter(events::session_id.eq(&session_id))
                .select(Event::as_select())
                .order(events::seq.desc())
                .limit(limit)
                .load(conn)?;

            events_vec.reverse();
            Ok(events_vec)
        })
        .await
    }

    /// Pull the most recent `scan_limit` events (by `ts` descending) across
    /// one or more sessions, optionally restricted to a set of event kinds.
    /// Returned newest-first. This is the SQL coarse filter behind the
    /// `search_worker_session` MCP tool — substring matching and
    /// errors-only refinement are applied by the caller in Rust so the
    /// grep semantics stay exact (case-insensitive literal `contains`)
    /// rather than depending on SQL `LIKE` wildcard/escaping behaviour.
    pub async fn search_session_events(
        &self,
        session_ids: Vec<String>,
        kinds: Option<Vec<String>>,
        scan_limit: i64,
    ) -> anyhow::Result<Vec<Event>> {
        if session_ids.is_empty() {
            return Ok(Vec::new());
        }
        self.with_conn(move |conn| {
            let mut query = events::table
                .filter(events::session_id.eq_any(session_ids))
                .into_boxed();
            if let Some(kinds) = kinds {
                query = query.filter(events::kind.eq_any(kinds));
            }
            query
                .select(Event::as_select())
                .order(events::ts.desc())
                .limit(scan_limit)
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Get a single event by its ID.
    pub async fn get_event(&self, event_id: &str) -> anyhow::Result<Option<Event>> {
        let event_id = event_id.to_string();
        self.with_conn(move |conn| {
            events::table
                .filter(events::id.eq(&event_id))
                .select(Event::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Get the most recent event of a given kind for a session, or None.
    ///
    /// Useful for "latest snapshot" event kinds like `todo`, where each event
    /// fully replaces the previous one and only the newest matters.
    pub async fn latest_event_of_kind(
        &self,
        session_id: &str,
        kind: &str,
    ) -> anyhow::Result<Option<Event>> {
        let session_id = session_id.to_string();
        let kind = kind.to_string();
        self.with_conn(move |conn| {
            events::table
                .filter(events::session_id.eq(&session_id))
                .filter(events::kind.eq(&kind))
                .select(Event::as_select())
                .order(events::seq.desc())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Get the latest seq number for a session, or None if no events exist.
    pub async fn latest_seq(&self, session_id: &str) -> anyhow::Result<Option<i32>> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            events::table
                .filter(events::session_id.eq(&session_id))
                .select(diesel::dsl::max(events::seq))
                .first::<Option<i32>>(conn)
                .map_err(Into::into)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use crate::db::Db;
    use crate::db::models::{NewEvent, NewFolder, NewSession};

    /// Create the folder + session row `"s1"` every test appends to.
    async fn seed(db: &Db) {
        let ts = chrono::Utc::now().to_rfc3339();

        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();

        db.create_session(NewSession {
            id: "s1".into(),
            name: "S".into(),
            folder_id: "f1".into(),
            model: None,
            effort: None,
            is_worker: false,
            project_id: None,
            card_id: None,
            conversation_id: None,
            created_at: ts.clone(),
            last_activity: ts,
            ..Default::default()
        })
        .await
        .unwrap();
    }

    async fn setup() -> Db {
        let db = Db::in_memory().unwrap();
        seed(&db).await;
        db
    }

    #[tokio::test]
    async fn test_append_auto_seq() {
        let db = setup().await;

        let e1 = db
            .append_event("s1", "user", serde_json::json!({"text": "hello"}))
            .await
            .unwrap();
        assert_eq!(e1.seq, 1);

        let e2 = db
            .append_event("s1", "agent-text", serde_json::json!({"text": "hi"}))
            .await
            .unwrap();
        assert_eq!(e2.seq, 2);

        let e3 = db
            .append_event("s1", "agent-end", serde_json::json!({"status": "complete"}))
            .await
            .unwrap();
        assert_eq!(e3.seq, 3);

        // Verify monotonic ordering
        assert!(e1.ts <= e2.ts);
        assert!(e2.ts <= e3.ts);
    }

    #[tokio::test]
    async fn test_append_event_blocking_persists_with_seq() {
        let db = setup().await;

        db.append_event_blocking("s1", "user", r#"{"text":"hi"}"#)
            .unwrap();
        db.append_event_blocking("s1", "agent-text", r#"{"text":"yo"}"#)
            .unwrap();

        // Reads back through the existing async path: two rows, seq 1 then 2.
        let tail = db.events_tail("s1", 10).await.unwrap();
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].seq, 1);
        assert_eq!(tail[0].kind, "user");
        assert_eq!(tail[0].data, r#"{"text":"hi"}"#);
        assert_eq!(tail[1].seq, 2);
        assert_eq!(tail[1].kind, "agent-text");
        assert_eq!(db.latest_seq("s1").await.unwrap(), Some(2));
    }

    #[tokio::test]
    async fn test_events_since() {
        let db = setup().await;

        for i in 1..=5 {
            db.append_event(
                "s1",
                "agent-text",
                serde_json::json!({"text": format!("chunk {i}")}),
            )
            .await
            .unwrap();
        }

        let since_2 = db.events_since("s1", 2).await.unwrap();
        assert_eq!(since_2.len(), 3);
        assert_eq!(since_2[0].seq, 3);
        assert_eq!(since_2[2].seq, 5);

        let since_0 = db.events_since("s1", 0).await.unwrap();
        assert_eq!(since_0.len(), 5);

        let since_5 = db.events_since("s1", 5).await.unwrap();
        assert_eq!(since_5.len(), 0);
    }

    #[tokio::test]
    async fn test_events_since_blocking() {
        let db = setup().await;

        for i in 1..=5 {
            db.append_event("s1", "agent-text", serde_json::json!({"n": i}))
                .await
                .unwrap();
        }

        // seq > 2, ordered ascending — the blocking twin of `events_since`.
        let since_2 = db.events_since_blocking("s1", 2, 200).unwrap();
        assert_eq!(since_2.len(), 3);
        assert_eq!(since_2[0].seq, 3);
        assert_eq!(since_2[2].seq, 5);

        // after_seq 0 → from the beginning.
        let all = db.events_since_blocking("s1", 0, 200).unwrap();
        assert_eq!(all.len(), 5);

        // `limit` caps the window, keeping the oldest matches first.
        let capped = db.events_since_blocking("s1", 0, 2).unwrap();
        assert_eq!(capped.len(), 2);
        assert_eq!(capped[0].seq, 1);
        assert_eq!(capped[1].seq, 2);

        // Nothing newer than the tail.
        let since_5 = db.events_since_blocking("s1", 5, 200).unwrap();
        assert_eq!(since_5.len(), 0);
    }

    #[tokio::test]
    async fn test_events_tail() {
        let db = setup().await;

        for i in 1..=10 {
            db.append_event("s1", "agent-text", serde_json::json!({"n": i}))
                .await
                .unwrap();
        }

        let tail = db.events_tail("s1", 3).await.unwrap();
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0].seq, 8);
        assert_eq!(tail[1].seq, 9);
        assert_eq!(tail[2].seq, 10);

        // Request more than exists
        let all = db.events_tail("s1", 100).await.unwrap();
        assert_eq!(all.len(), 10);
        assert_eq!(all[0].seq, 1);
    }

    #[tokio::test]
    async fn test_latest_seq() {
        let db = setup().await;

        let empty = db.latest_seq("s1").await.unwrap();
        assert_eq!(empty, None);

        db.append_event("s1", "user", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(db.latest_seq("s1").await.unwrap(), Some(1));

        db.append_event("s1", "agent-text", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(db.latest_seq("s1").await.unwrap(), Some(2));
    }

    /// Concurrent appenders must not collide on `seq`. The old
    /// read-then-insert shape had both racers compute the same next seq, and
    /// the loser's event was dropped on the `UNIQUE(session_id, seq)`
    /// violation. Every task's event has to land, with a unique number.
    #[tokio::test]
    async fn test_concurrent_appends_assign_unique_seqs() {
        const N: usize = 64;
        let db = setup().await;

        let mut handles = Vec::with_capacity(N);
        for i in 0..N {
            let db = db.clone();
            handles.push(tokio::spawn(async move {
                db.append_event("s1", "user", serde_json::json!({ "i": i }))
                    .await
                    .unwrap()
            }));
        }

        let mut seqs: Vec<i32> = Vec::with_capacity(N);
        for h in handles {
            seqs.push(h.await.unwrap().seq);
        }
        seqs.sort_unstable();

        // Nothing dropped, nothing duplicated: exactly 1..=N.
        assert_eq!(seqs, (1..=N as i32).collect::<Vec<_>>());
        assert_eq!(db.events_since("s1", 0).await.unwrap().len(), N);
        assert_eq!(db.latest_seq("s1").await.unwrap(), Some(N as i32));
    }

    /// Same race through the synchronous plugin-host path, which shares the
    /// atomic insert. `append_event_blocking` locks the connection on the
    /// calling thread, so drive it from blocking tasks.
    #[tokio::test]
    async fn test_concurrent_blocking_appends_assign_unique_seqs() {
        const N: usize = 16;
        let db = setup().await;

        let mut handles = Vec::with_capacity(N);
        for _ in 0..N {
            let db = db.clone();
            handles.push(tokio::task::spawn_blocking(move || {
                db.append_event_blocking("s1", "user", r#"{"text":"hi"}"#)
                    .unwrap()
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let stored = db.events_since("s1", 0).await.unwrap();
        assert_eq!(stored.len(), N);
        let seqs: Vec<i32> = stored.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, (1..=N as i32).collect::<Vec<_>>());
    }

    /// The seq is derived inside the INSERT from the committed rows, so it
    /// continues past a row written with an out-of-band seq instead of
    /// colliding with it.
    #[tokio::test]
    async fn test_append_continues_after_seq_gap() {
        let db = setup().await;

        db.create_event(NewEvent {
            id: "manual".into(),
            session_id: "s1".into(),
            seq: 42,
            ts: 1,
            kind: "user".into(),
            data: "{}".into(),
        })
        .await
        .unwrap();

        let next = db
            .append_event("s1", "user", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(next.seq, 43);
    }

    /// Two `Db` handles on the same file are two SQLite connections with
    /// independent mutexes — the shape where a seq collision is genuinely
    /// reachable. Appends through either must share one dense seq space.
    #[tokio::test]
    async fn test_appends_across_connections_share_one_seq_space() {
        let dir = tempfile::tempdir().unwrap();
        let a = Db::open(dir.path()).unwrap();
        seed(&a).await;
        let b = Db::open(dir.path()).unwrap();

        let mut seqs = Vec::new();
        for i in 0..6 {
            let db = if i % 2 == 0 { &a } else { &b };
            seqs.push(
                db.append_event("s1", "user", serde_json::json!({ "i": i }))
                    .await
                    .unwrap()
                    .seq,
            );
        }
        assert_eq!(seqs, vec![1, 2, 3, 4, 5, 6]);
    }

    /// `events_since_page` is the WS resume pager: ordered ascending, capped,
    /// and resumable from the last seq it returned.
    #[tokio::test]
    async fn test_events_since_page_paginates() {
        let db = setup().await;
        for i in 0..7 {
            db.append_event("s1", "user", serde_json::json!({ "i": i }))
                .await
                .unwrap();
        }

        let page1 = db.events_since_page("s1", 0, 3).await.unwrap();
        assert_eq!(
            page1.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );

        let page2 = db.events_since_page("s1", 3, 3).await.unwrap();
        assert_eq!(
            page2.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![4, 5, 6]
        );

        // Short page = caught up.
        let page3 = db.events_since_page("s1", 6, 3).await.unwrap();
        assert_eq!(page3.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![7]);
        assert!(db.events_since_page("s1", 7, 3).await.unwrap().is_empty());
    }
}
