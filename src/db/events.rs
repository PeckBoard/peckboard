use diesel::prelude::*;
use std::time::{SystemTime, UNIX_EPOCH};

use super::Db;
use super::models::{Event, NewEvent};
use super::schema::events;

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

        self.with_conn(move |conn| {
            // Get the next seq for this session.
            let next_seq: i32 = events::table
                .filter(events::session_id.eq(&session_id))
                .select(diesel::dsl::max(events::seq))
                .first::<Option<i32>>(conn)?
                .map(|s| s + 1)
                .unwrap_or(1);

            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64;

            let new_event = NewEvent {
                id: uuid::Uuid::new_v4().to_string(),
                session_id,
                seq: next_seq,
                ts,
                kind,
                data: data_str,
            };

            diesel::insert_into(events::table)
                .values(&new_event)
                .returning(Event::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Synchronous twin of [`append_event`], for WASM plugin host
    /// functions that run inside a blocking extism call. Inserts ONE event
    /// row with the same seq/id/ts scheme as the async path — `seq` is the
    /// per-session `max(seq) + 1` (or 1 for the first), `id` a fresh uuid,
    /// `ts` millis since the Unix epoch. `data` is stored verbatim (already
    /// JSON-encoded by the caller). Does NOT broadcast.
    pub(crate) fn append_event_blocking(
        &self,
        session_id: &str,
        kind: &str,
        data: &str,
    ) -> anyhow::Result<()> {
        let session_id = session_id.to_string();
        let kind = kind.to_string();
        let data = data.to_string();

        self.with_conn_blocking(move |conn| {
            // Get the next seq for this session.
            let next_seq: i32 = events::table
                .filter(events::session_id.eq(&session_id))
                .select(diesel::dsl::max(events::seq))
                .first::<Option<i32>>(conn)?
                .map(|s| s + 1)
                .unwrap_or(1);

            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64;

            let new_event = NewEvent {
                id: uuid::Uuid::new_v4().to_string(),
                session_id,
                seq: next_seq,
                ts,
                kind,
                data,
            };

            diesel::insert_into(events::table)
                .values(&new_event)
                .execute(conn)?;
            Ok(())
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
    use crate::db::models::{NewFolder, NewSession};

    async fn setup() -> Db {
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
}
