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
    /// session/project/report/repeating_task, the corresponding tab
    /// moves to the front.
    ///
    /// Returns `Ok(None)` if the referenced item does not exist (or
    /// `item_type` is unknown). `user_tabs` is polymorphic so there's
    /// no FK to lean on; without this check a stale URL like
    /// `/sessions/<deleted-id>` (or a cross-device delete race) would
    /// write an orphan row that then renders as a phantom chip.
    ///
    /// For DB-backed kinds (session/project/repeating_task) the
    /// existence check is a `SELECT id` against the owning table. The
    /// file-backed `report` kind has no DB row to check; callers must
    /// pre-validate the report file exists before calling.
    pub async fn upsert_user_tab(
        &self,
        user_id: &str,
        item_type: &str,
        item_id: &str,
    ) -> anyhow::Result<Option<UserTab>> {
        let now = chrono::Utc::now().to_rfc3339();
        let tab = UserTab {
            user_id: user_id.to_string(),
            item_type: item_type.to_string(),
            item_id: item_id.to_string(),
            last_active: now,
        };
        self.with_conn(move |conn| {
            let exists = match tab.item_type.as_str() {
                "session" => sessions::table
                    .find(&tab.item_id)
                    .select(sessions::id)
                    .first::<String>(conn)
                    .optional()?
                    .is_some(),
                "project" => projects::table
                    .find(&tab.item_id)
                    .select(projects::id)
                    .first::<String>(conn)
                    .optional()?
                    .is_some(),
                "repeating_task" => repeating_tasks::table
                    .find(&tab.item_id)
                    .select(repeating_tasks::id)
                    .first::<String>(conn)
                    .optional()?
                    .is_some(),
                // Reports live on disk, not in the DB. The caller validates
                // existence; the DB layer trusts the route to have done so.
                "report" => true,
                _ => false,
            };
            if !exists {
                return Ok::<Option<UserTab>, anyhow::Error>(None);
            }
            let row = diesel::insert_into(user_tabs::table)
                .values(&tab)
                .on_conflict((user_tabs::user_id, user_tabs::item_type, user_tabs::item_id))
                .do_update()
                .set(user_tabs::last_active.eq(&tab.last_active))
                .returning(UserTab::as_returning())
                .get_result(conn)?;
            Ok(Some(row))
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

    /// Drop every user_tabs row pointing at the given (item_type, item_id),
    /// across all users. Used by cascade deletes for polymorphic tab
    /// kinds (session/project/repeating_task) so the strip doesn't
    /// render orphan chips after a delete. Reports are file-backed and
    /// have no delete path, so they don't go through this helper.
    pub async fn delete_user_tabs_for_item(
        &self,
        item_type: &str,
        item_id: &str,
    ) -> anyhow::Result<usize> {
        let item_type = item_type.to_string();
        let item_id = item_id.to_string();
        self.with_conn(move |conn| {
            let count = diesel::delete(
                user_tabs::table
                    .filter(user_tabs::item_type.eq(&item_type))
                    .filter(user_tabs::item_id.eq(&item_id)),
            )
            .execute(conn)?;
            Ok(count)
        })
        .await
    }

    /// Count `user_tabs` rows (across ALL users) pointing at the given
    /// (item_type, item_id). Powers the temp-session rule: the session is
    /// deleted only when the LAST tab pointing at it goes away, so another
    /// user who still has the tab open keeps the session alive.
    pub async fn count_user_tabs_for_item(
        &self,
        item_type: &str,
        item_id: &str,
    ) -> anyhow::Result<i64> {
        let item_type = item_type.to_string();
        let item_id = item_id.to_string();
        self.with_conn(move |conn| {
            user_tabs::table
                .filter(user_tabs::item_type.eq(&item_type))
                .filter(user_tabs::item_id.eq(&item_id))
                .count()
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }
}
