use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    pub async fn create_session(&self, new: NewSession) -> anyhow::Result<Session> {
        self.with_conn(move |conn| {
            diesel::insert_into(sessions::table)
                .values(&new)
                .returning(Session::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_session(&self, id: &str) -> anyhow::Result<Option<Session>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            sessions::table
                .find(&id)
                .select(Session::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_sessions(&self) -> anyhow::Result<Vec<Session>> {
        self.with_conn(move |conn| {
            sessions::table
                .select(Session::as_select())
                .order(sessions::last_activity.desc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_sessions_by_folder(&self, folder_id: &str) -> anyhow::Result<Vec<Session>> {
        let folder_id = folder_id.to_string();
        self.with_conn(move |conn| {
            sessions::table
                .filter(sessions::folder_id.eq(&folder_id))
                .select(Session::as_select())
                .order(sessions::last_activity.desc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Move all sessions from one folder to another.
    pub async fn move_sessions_to_folder(
        &self,
        from_folder_id: &str,
        to_folder_id: &str,
    ) -> anyhow::Result<usize> {
        let from = from_folder_id.to_string();
        let to = to_folder_id.to_string();
        self.with_conn(move |conn| {
            diesel::update(sessions::table.filter(sessions::folder_id.eq(&from)))
                .set(sessions::folder_id.eq(&to))
                .execute(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// List worker sessions for a specific project.
    pub async fn list_worker_sessions_by_project(
        &self,
        project_id: &str,
    ) -> anyhow::Result<Vec<Session>> {
        let project_id = project_id.to_string();
        self.with_conn(move |conn| {
            sessions::table
                .filter(sessions::is_worker.eq(true))
                .filter(sessions::project_id.eq(&project_id))
                .select(Session::as_select())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_worker_sessions(&self) -> anyhow::Result<Vec<Session>> {
        self.with_conn(move |conn| {
            sessions::table
                .filter(sessions::is_worker.eq(true))
                .select(Session::as_select())
                .order(sessions::last_activity.desc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_plain_sessions(&self) -> anyhow::Result<Vec<Session>> {
        self.with_conn(move |conn| {
            sessions::table
                .filter(sessions::is_worker.eq(false))
                .select(Session::as_select())
                .order(sessions::last_activity.desc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_plain_sessions_by_folder(
        &self,
        folder_id: &str,
    ) -> anyhow::Result<Vec<Session>> {
        let folder_id = folder_id.to_string();
        self.with_conn(move |conn| {
            sessions::table
                .filter(sessions::folder_id.eq(&folder_id))
                .filter(sessions::is_worker.eq(false))
                .select(Session::as_select())
                .order(sessions::last_activity.desc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn update_session(
        &self,
        id: &str,
        update: UpdateSession,
    ) -> anyhow::Result<Option<Session>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            diesel::update(sessions::table.find(&id))
                .set(&update)
                .returning(Session::as_returning())
                .get_result(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn delete_session(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            // Drop any user_tabs entries pointing at this session before
            // deleting the session itself — there's no FK cascade since
            // user_tabs is polymorphic (item_type + item_id), and the
            // frontend tab strip would otherwise render orphan chips
            // labelled "Session" when its name lookup misses.
            diesel::delete(
                user_tabs::table
                    .filter(user_tabs::item_type.eq("session"))
                    .filter(user_tabs::item_id.eq(&id)),
            )
            .execute(conn)?;
            let count = diesel::delete(sessions::table.find(&id)).execute(conn)?;
            Ok(count > 0)
        })
        .await
    }
}
