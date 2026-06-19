use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    pub async fn create_announcement(&self, new: NewAnnouncement) -> anyhow::Result<Announcement> {
        self.with_conn(move |conn| {
            diesel::insert_into(announcements::table)
                .values(&new)
                .returning(Announcement::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_announcements(&self) -> anyhow::Result<Vec<Announcement>> {
        self.with_conn(move |conn| {
            announcements::table
                .select(Announcement::as_select())
                .order(announcements::created_at.desc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn delete_announcement(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let count = diesel::delete(announcements::table.find(&id)).execute(conn)?;
            Ok(count > 0)
        })
        .await
    }
}
