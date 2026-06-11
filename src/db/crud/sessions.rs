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

    /// Every session bound to a project — workers AND experts, in
    /// undefined order. Used by the change-folder route to drag a
    /// project's entire session set into the new folder in one shot.
    pub async fn list_sessions_by_project(&self, project_id: &str) -> anyhow::Result<Vec<Session>> {
        let project_id = project_id.to_string();
        self.with_conn(move |conn| {
            sessions::table
                .filter(sessions::project_id.eq(&project_id))
                .select(Session::as_select())
                .load(conn)
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

    /// Plain (non-worker, non-expert) sessions — the ordinary chat list.
    /// Experts are deliberately excluded: they must never surface in the
    /// normal chat session list.
    pub async fn list_plain_sessions(&self) -> anyhow::Result<Vec<Session>> {
        self.with_conn(move |conn| {
            sessions::table
                .filter(sessions::is_worker.eq(false))
                .filter(sessions::is_expert.eq(false))
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
                .filter(sessions::is_expert.eq(false))
                .select(Session::as_select())
                .order(sessions::last_activity.desc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Keyset-paginated page of "plain" (non-worker, non-expert) sessions,
    /// newest activity first. `before` is the cursor returned from the
    /// previous page's tail: pass `Some((last_activity, id))` to get the
    /// next page, `None` for the first page.
    ///
    /// Tuple-keyset (`(la, id) < (cursor_la, cursor_id)` in descending
    /// order) is stable across concurrent inserts: a session whose
    /// `last_activity` bumps mid-scroll just shifts up to a position
    /// the caller already loaded — it never reappears further down,
    /// and never gets skipped. `id` is the secondary key so rows that
    /// share a `last_activity` (rare but possible — e.g. a bulk
    /// import) are still totally ordered.
    pub async fn list_plain_sessions_page(
        &self,
        before: Option<(String, String)>,
        limit: i64,
    ) -> anyhow::Result<Vec<Session>> {
        self.with_conn(move |conn| {
            let mut query = sessions::table
                .filter(sessions::is_worker.eq(false))
                .filter(sessions::is_expert.eq(false))
                .into_boxed();
            if let Some((cursor_la, cursor_id)) = before {
                let la_for_eq = cursor_la.clone();
                query = query.filter(
                    sessions::last_activity
                        .lt(cursor_la)
                        .or(sessions::last_activity
                            .eq(la_for_eq)
                            .and(sessions::id.lt(cursor_id))),
                );
            }
            query
                .order((sessions::last_activity.desc(), sessions::id.desc()))
                .limit(limit)
                .select(Session::as_select())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Folder-scoped variant of [`list_plain_sessions_page`].
    pub async fn list_plain_sessions_by_folder_page(
        &self,
        folder_id: &str,
        before: Option<(String, String)>,
        limit: i64,
    ) -> anyhow::Result<Vec<Session>> {
        let folder_id = folder_id.to_string();
        self.with_conn(move |conn| {
            let mut query = sessions::table
                .filter(sessions::folder_id.eq(&folder_id))
                .filter(sessions::is_worker.eq(false))
                .filter(sessions::is_expert.eq(false))
                .into_boxed();
            if let Some((cursor_la, cursor_id)) = before {
                let la_for_eq = cursor_la.clone();
                query = query.filter(
                    sessions::last_activity
                        .lt(cursor_la)
                        .or(sessions::last_activity
                            .eq(la_for_eq)
                            .and(sessions::id.lt(cursor_id))),
                );
            }
            query
                .order((sessions::last_activity.desc(), sessions::id.desc()))
                .limit(limit)
                .select(Session::as_select())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// All expert sessions, newest activity first.
    pub async fn list_expert_sessions(&self) -> anyhow::Result<Vec<Session>> {
        self.with_conn(move |conn| {
            sessions::table
                .filter(sessions::is_expert.eq(true))
                .select(Session::as_select())
                .order(sessions::last_activity.desc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Expert sessions owned by a specific project.
    pub async fn list_expert_sessions_by_project(
        &self,
        project_id: &str,
    ) -> anyhow::Result<Vec<Session>> {
        let project_id = project_id.to_string();
        self.with_conn(move |conn| {
            sessions::table
                .filter(sessions::is_expert.eq(true))
                .filter(sessions::project_id.eq(&project_id))
                .select(Session::as_select())
                .order(sessions::last_activity.desc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Experts a session in `project_id` may consult: ones scoped to that
    /// project plus globally-scoped experts (`project_id IS NULL`).
    pub async fn list_expert_sessions_by_scope(
        &self,
        project_id: &str,
    ) -> anyhow::Result<Vec<Session>> {
        let project_id = project_id.to_string();
        self.with_conn(move |conn| {
            sessions::table
                .filter(sessions::is_expert.eq(true))
                .filter(
                    sessions::project_id
                        .eq(&project_id)
                        .or(sessions::project_id.is_null()),
                )
                .select(Session::as_select())
                .order(sessions::last_activity.desc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Fetch an expert session by its (stable) id. Returns `None` if the
    /// id doesn't exist or the session isn't an expert.
    pub async fn get_expert_session(&self, id: &str) -> anyhow::Result<Option<Session>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            sessions::table
                .find(&id)
                .filter(sessions::is_expert.eq(true))
                .select(Session::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Insert a permanent (stable-id) expert if it doesn't yet exist;
    /// otherwise return the existing row untouched. This is how the
    /// question- and PM-experts rehydrate under their stable ids across
    /// restarts without clobbering the accumulated session. The caller is
    /// expected to set `is_expert`, `is_permanent`, and `expert_kind` on
    /// `new`.
    pub async fn upsert_permanent_expert(&self, new: NewSession) -> anyhow::Result<Session> {
        self.with_conn(move |conn| {
            let id = new.id.clone();
            diesel::insert_or_ignore_into(sessions::table)
                .values(&new)
                .execute(conn)?;
            sessions::table
                .find(&id)
                .select(Session::as_select())
                .first(conn)
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
            diesel::delete(todos::table.filter(todos::session_id.eq(&id))).execute(conn)?;
            let count = diesel::delete(sessions::table.find(&id)).execute(conn)?;
            Ok(count > 0)
        })
        .await
    }
}
