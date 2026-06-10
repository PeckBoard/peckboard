use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    pub async fn create_event(&self, new: NewEvent) -> anyhow::Result<Event> {
        self.with_conn(move |conn| {
            diesel::insert_into(events::table)
                .values(&new)
                .returning(Event::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_events_by_session(
        &self,
        session_id: &str,
        after_seq: Option<i32>,
    ) -> anyhow::Result<Vec<Event>> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            let mut query = events::table
                .filter(events::session_id.eq(&session_id))
                .into_boxed();

            if let Some(seq) = after_seq {
                query = query.filter(events::seq.gt(seq));
            }

            query
                .select(Event::as_select())
                .order(events::seq.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Last `limit` events for a session, oldest-first, strictly older
    /// than `before_seq` (or the absolute newest if `before_seq` is
    /// `None`). Returned in ascending `seq` order so the UI can render
    /// them as a contiguous earlier page without re-sorting.
    ///
    /// This is the "load older" path for the chat view: default fetch
    /// pulls the latest N events; subsequent calls use the lowest seq
    /// from the loaded list as `before_seq` to walk backwards through
    /// history a page at a time. Uses the existing `idx_events_session`
    /// `(session_id, seq)` index — no new index required.
    pub async fn list_events_by_session_before(
        &self,
        session_id: &str,
        before_seq: Option<i32>,
        limit: i64,
    ) -> anyhow::Result<Vec<Event>> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            let mut query = events::table
                .filter(events::session_id.eq(&session_id))
                .into_boxed();
            if let Some(seq) = before_seq {
                query = query.filter(events::seq.lt(seq));
            }
            let mut rows: Vec<Event> = query
                .select(Event::as_select())
                .order(events::seq.desc())
                .limit(limit)
                .load(conn)?;
            // Walked the index backwards to grab "the newest N"; flip
            // back to ascending so the caller can splice the page in
            // front of the existing buffer without re-sorting.
            rows.reverse();
            Ok(rows)
        })
        .await
    }

    /// Lifecycle events across every session that has ever been assigned
    /// to this card, ordered oldest-first by wall-clock `ts` (per-session
    /// `seq` is monotonic only within a session so it can't order
    /// cross-session events). Used by the auto-pause counter, which
    /// recognizes three reset markers — `agent-end status=complete`,
    /// `step-change`, and the resume sentinel `auto-pause-cleared`
    /// (see `pipeline::PAUSE_CLEARED_KIND`).
    pub async fn card_lifecycle_events(
        &self,
        card_id: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<Event>> {
        let card_id = card_id.to_string();
        self.with_conn(move |conn| {
            let mut rows: Vec<Event> = events::table
                .inner_join(sessions::table.on(sessions::id.eq(events::session_id)))
                .filter(sessions::card_id.eq(&card_id))
                .filter(events::kind.eq_any([
                    "agent-end",
                    "step-change",
                    crate::worker::pipeline::PAUSE_CLEARED_KIND,
                ]))
                .select(Event::as_select())
                .order((events::ts.desc(), events::seq.desc()))
                .limit(limit)
                .load(conn)?;
            rows.reverse();
            Ok(rows)
        })
        .await
    }

    pub async fn delete_events_by_session(&self, session_id: &str) -> anyhow::Result<usize> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            diesel::delete(events::table.filter(events::session_id.eq(&session_id)))
                .execute(conn)
                .map_err(Into::into)
        })
        .await
    }
}
