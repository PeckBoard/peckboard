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
}
