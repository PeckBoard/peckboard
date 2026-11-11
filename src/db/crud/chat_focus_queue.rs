use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    /// Persist one held message. Blocking: the plugin host's
    /// `send_message_impl` decides to hold on its own (sync) thread.
    pub(crate) fn insert_chat_focus_item_blocking(
        &self,
        item: ChatFocusItem,
    ) -> anyhow::Result<()> {
        self.with_conn_blocking(move |conn| {
            diesel::insert_into(chat_focus_queue::table)
                .values(&item)
                .execute(conn)?;
            Ok(())
        })
    }

    /// Held messages for one session, oldest first.
    pub async fn list_chat_focus_items(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Vec<ChatFocusItem>> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            chat_focus_queue::table
                .filter(chat_focus_queue::session_id.eq(&session_id))
                .select(ChatFocusItem::as_select())
                .order((
                    chat_focus_queue::created_at.asc(),
                    chat_focus_queue::id.asc(),
                ))
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Sessions that have held messages (the release ticker's work list).
    pub async fn chat_focus_sessions(&self) -> anyhow::Result<Vec<String>> {
        self.with_conn(move |conn| {
            chat_focus_queue::table
                .select(chat_focus_queue::session_id)
                .distinct()
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Drop delivered messages. Missing ids are ignored.
    pub async fn delete_chat_focus_items(&self, ids: Vec<String>) -> anyhow::Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        self.with_conn(move |conn| {
            diesel::delete(chat_focus_queue::table.filter(chat_focus_queue::id.eq_any(&ids)))
                .execute(conn)
                .map_err(Into::into)
        })
        .await
    }
}
