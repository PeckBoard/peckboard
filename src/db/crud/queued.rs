use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    pub async fn upsert_queued_message(
        &self,
        new: NewQueuedMessage,
    ) -> anyhow::Result<QueuedMessage> {
        self.with_conn(move |conn| {
            diesel::replace_into(queued_messages::table)
                .values(&new)
                .returning(QueuedMessage::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_queued_message(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Option<QueuedMessage>> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            queued_messages::table
                .find(&session_id)
                .select(QueuedMessage::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn delete_queued_message(&self, session_id: &str) -> anyhow::Result<bool> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            let count = diesel::delete(queued_messages::table.find(&session_id)).execute(conn)?;
            Ok(count > 0)
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
}
