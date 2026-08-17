use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    /// Append a message to the session's FIFO queue. Every message queued
    /// while the agent is busy is kept — delivery is oldest-first, one
    /// agent turn per message, driven by the completion listener.
    pub async fn enqueue_message(&self, new: NewQueuedMessage) -> anyhow::Result<QueuedMessage> {
        self.with_conn(move |conn| {
            diesel::insert_into(queued_messages::table)
                .values(&new)
                .returning(QueuedMessage::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// All queued messages for a session, oldest first (delivery order).
    pub async fn list_queued_messages(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Vec<QueuedMessage>> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            queued_messages::table
                .filter(queued_messages::session_id.eq(&session_id))
                .order(queued_messages::id.asc())
                .select(QueuedMessage::as_select())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// The next message the drain would deliver (oldest), if any.
    pub async fn next_queued_message(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Option<QueuedMessage>> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            queued_messages::table
                .filter(queued_messages::session_id.eq(&session_id))
                .order(queued_messages::id.asc())
                .select(QueuedMessage::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Fetch one queued message by id, scoped to the session so a route
    /// can't force/delete another session's row through a guessed id.
    pub async fn get_queued_message_by_id(
        &self,
        session_id: &str,
        message_id: i64,
    ) -> anyhow::Result<Option<QueuedMessage>> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            queued_messages::table
                .filter(queued_messages::session_id.eq(&session_id))
                .filter(queued_messages::id.eq(message_id))
                .select(QueuedMessage::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }
    /// Record that the `user` event for this queued row has been written
    /// to the transcript. The drain appends that event for machine-queued
    /// rows *before* dispatch, and dispatch can fail — the row then stays
    /// queued for a retry, which must not append a second copy. Returns
    /// false if the row is already gone.
    pub async fn mark_queued_message_user_event_appended(
        &self,
        session_id: &str,
        message_id: i64,
    ) -> anyhow::Result<bool> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            let count = diesel::update(
                queued_messages::table
                    .filter(queued_messages::session_id.eq(&session_id))
                    .filter(queued_messages::id.eq(message_id)),
            )
            .set(queued_messages::user_event_appended.eq(true))
            .execute(conn)?;
            Ok(count > 0)
        })
        .await
    }

    /// Remove one queued message. Returns false if it was already gone
    /// (e.g. the drain delivered it between the click and the request).
    pub async fn delete_queued_message_by_id(
        &self,
        session_id: &str,
        message_id: i64,
    ) -> anyhow::Result<bool> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            let count = diesel::delete(
                queued_messages::table
                    .filter(queued_messages::session_id.eq(&session_id))
                    .filter(queued_messages::id.eq(message_id)),
            )
            .execute(conn)?;
            Ok(count > 0)
        })
        .await
    }

    /// Drop every queued message for a session. Returns the number removed.
    pub async fn clear_queued_messages(&self, session_id: &str) -> anyhow::Result<usize> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            diesel::delete(
                queued_messages::table.filter(queued_messages::session_id.eq(&session_id)),
            )
            .execute(conn)
            .map_err(Into::into)
        })
        .await
    }

    /// Delete every queued message belonging to a worker session of the
    /// given project. Used on project pause so the cancel's completion
    /// listener doesn't drain a buffered message into a fresh agent run
    /// — pause means "stop the work", not "stop new work then deliver the
    /// pending one anyway".
    ///
    /// Returns the number of rows deleted. Safe on a project with no
    /// worker sessions (returns 0).
    pub async fn delete_queued_messages_for_project(
        &self,
        project_id: &str,
    ) -> anyhow::Result<usize> {
        let project_id = project_id.to_string();
        self.with_conn(move |conn| {
            use crate::db::schema::sessions;
            // Diesel doesn't expose a portable cross-table delete; do it
            // as a single subquery so we don't load the session list back
            // into Rust just to fan-out N deletes.
            let worker_session_ids: Vec<String> = sessions::table
                .filter(sessions::is_worker.eq(true))
                .filter(sessions::project_id.eq(&project_id))
                .select(sessions::id)
                .load(conn)?;
            if worker_session_ids.is_empty() {
                return Ok(0);
            }
            diesel::delete(
                queued_messages::table
                    .filter(queued_messages::session_id.eq_any(&worker_session_ids)),
            )
            .execute(conn)
            .map_err(Into::into)
        })
        .await
    }

    /// Distinct session ids that currently hold at least one queued
    /// message. Small by construction — a row only exists while a message
    /// is undelivered — so this is much cheaper than walking every
    /// session. Auth recovery uses it to find the turns parked waiting on
    /// a working login (see [`crate::provider::auth_recovery`]).
    pub async fn sessions_with_queued_messages(&self) -> anyhow::Result<Vec<String>> {
        self.with_conn(move |conn| {
            queued_messages::table
                .select(queued_messages::session_id)
                .distinct()
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }
}
