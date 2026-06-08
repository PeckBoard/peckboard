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
