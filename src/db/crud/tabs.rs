use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    /// List a user's tabs in most-recently-active first order.
    pub async fn list_user_tabs(&self, user_id: &str) -> anyhow::Result<Vec<UserTab>> {
        let user_id = user_id.to_string();
        self.with_conn(move |conn| {
            user_tabs::table
                .filter(user_tabs::user_id.eq(&user_id))
                .select(UserTab::as_select())
                .order(user_tabs::last_active.desc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Insert a tab, or bump its `last_active` if it already exists.
    /// This is what powers MRU ordering: every time the user opens a
    /// session/project, the corresponding tab moves to the front.
    pub async fn upsert_user_tab(
        &self,
        user_id: &str,
        item_type: &str,
        item_id: &str,
    ) -> anyhow::Result<UserTab> {
        let now = chrono::Utc::now().to_rfc3339();
        let tab = UserTab {
            user_id: user_id.to_string(),
            item_type: item_type.to_string(),
            item_id: item_id.to_string(),
            last_active: now,
        };
        self.with_conn(move |conn| {
            diesel::insert_into(user_tabs::table)
                .values(&tab)
                .on_conflict((user_tabs::user_id, user_tabs::item_type, user_tabs::item_id))
                .do_update()
                .set(user_tabs::last_active.eq(&tab.last_active))
                .returning(UserTab::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn delete_user_tab(
        &self,
        user_id: &str,
        item_type: &str,
        item_id: &str,
    ) -> anyhow::Result<bool> {
        let user_id = user_id.to_string();
        let item_type = item_type.to_string();
        let item_id = item_id.to_string();
        self.with_conn(move |conn| {
            let count = diesel::delete(
                user_tabs::table
                    .filter(user_tabs::user_id.eq(&user_id))
                    .filter(user_tabs::item_type.eq(&item_type))
                    .filter(user_tabs::item_id.eq(&item_id)),
            )
            .execute(conn)?;
            Ok(count > 0)
        })
        .await
    }
}
