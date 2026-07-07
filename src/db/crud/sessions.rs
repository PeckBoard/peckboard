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

    /// Resolve the owner (`user_id`) for a session spawned internally, when no
    /// authenticated user is directly in scope. Order:
    /// 1. Inherit the owner of `parent_session_id` when that session has one.
    /// 2. Else, if the install holds exactly one user, that user (the same rule
    ///    the ownership backfill applies to legacy rows).
    /// 3. Else `None` -- ambiguous multi-user install; ownership left unknown
    ///    and treated as non-matching by the same-user send_message gate.
    pub async fn resolve_spawned_session_owner(
        &self,
        parent_session_id: Option<&str>,
    ) -> Option<String> {
        if let Some(pid) = parent_session_id
            && let Ok(Some(sess)) = self.get_session(pid).await
            && sess.user_id.is_some()
        {
            return sess.user_id;
        }
        self.sole_user_id().await
    }

    /// The single user's id when the install has exactly one user, else `None`.
    pub async fn sole_user_id(&self) -> Option<String> {
        self.with_conn(move |conn| {
            let ids: Vec<String> = users::table.select(users::id).limit(2).load(conn)?;
            Ok::<_, anyhow::Error>(ids)
        })
        .await
        .ok()
        .and_then(|ids| (ids.len() == 1).then(|| ids.into_iter().next().unwrap()))
    }

    /// Synchronous twin of [`Db::resolve_spawned_session_owner`], for the
    /// blocking plugin-host call path.
    pub(crate) fn resolve_spawned_session_owner_blocking(
        &self,
        parent_session_id: Option<&str>,
    ) -> Option<String> {
        self.with_conn_blocking(move |conn| {
            if let Some(pid) = parent_session_id {
                let owner: Option<Option<String>> = sessions::table
                    .find(pid)
                    .select(sessions::user_id)
                    .first::<Option<String>>(conn)
                    .optional()?;
                if let Some(Some(uid)) = owner {
                    return Ok(Some(uid));
                }
            }
            let ids: Vec<String> = users::table.select(users::id).limit(2).load(conn)?;
            Ok::<_, anyhow::Error>((ids.len() == 1).then(|| ids.into_iter().next().unwrap()))
        })
        .ok()
        .flatten()
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

    /// Set (or clear, with `None`) a session's custom system prompt. Returns
    /// the updated session, or `None` if no session has that id.
    /// Set (or clear) a session's custom system prompt body, plus the
    /// name of the library prompt it came from. Pass `name = Some(_)` when the
    /// body was resolved from a named library prompt, or `None` when the body
    /// is a raw string or is being cleared — the reference column stays
    /// consistent with the resolved body either way.
    pub async fn set_session_system_prompt(
        &self,
        id: &str,
        prompt: Option<String>,
        name: Option<String>,
    ) -> anyhow::Result<Option<Session>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            diesel::update(sessions::table.find(&id))
                .set((
                    sessions::system_prompt.eq(prompt),
                    sessions::system_prompt_name.eq(name),
                ))
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

    /// Synchronous twin of [`create_session`], for WASM plugin host
    /// functions that run inside a blocking extism call. Same insert +
    /// return-the-row logic.
    pub(crate) fn create_session_blocking(&self, new: NewSession) -> anyhow::Result<Session> {
        self.with_conn_blocking(move |conn| {
            diesel::insert_into(sessions::table)
                .values(&new)
                .returning(Session::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
    }

    /// Synchronous twin of [`get_session`].
    pub(crate) fn get_session_blocking(&self, id: &str) -> anyhow::Result<Option<Session>> {
        let id = id.to_string();
        self.with_conn_blocking(move |conn| {
            sessions::table
                .find(&id)
                .select(Session::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
    }

    /// Blocking twin of [`list_sessions`]: every session, newest first. Used
    /// by the session-control host functions, which run on a blocking thread
    /// and deliberately span all folders (no visibility boundary).
    pub(crate) fn list_sessions_blocking(&self) -> anyhow::Result<Vec<Session>> {
        self.with_conn_blocking(move |conn| {
            sessions::table
                .select(Session::as_select())
                .order(sessions::last_activity.desc())
                .load(conn)
                .map_err(Into::into)
        })
    }

    /// Synchronous twin of [`update_session`].
    pub(crate) fn update_session_blocking(
        &self,
        id: &str,
        update: UpdateSession,
    ) -> anyhow::Result<Option<Session>> {
        let id = id.to_string();
        self.with_conn_blocking(move |conn| {
            diesel::update(sessions::table.find(&id))
                .set(&update)
                .returning(Session::as_returning())
                .get_result(conn)
                .optional()
                .map_err(Into::into)
        })
    }
}

#[cfg(test)]
mod tests {
    use diesel::prelude::*;

    use crate::db::Db;
    use crate::db::models::{NewFolder, NewSession, UpdateSession};

    fn seed_folder(db: &Db) {
        let ts = chrono::Utc::now().to_rfc3339();
        db.with_conn_blocking(move |conn| {
            use crate::db::schema::folders;
            diesel::insert_into(folders::table)
                .values(&NewFolder {
                    id: "f1".into(),
                    name: "F".into(),
                    path: "/tmp/f".into(),
                    created_at: ts,
                })
                .execute(conn)?;
            Ok(())
        })
        .unwrap();
    }

    fn new_session(id: &str, project_id: Option<&str>, last_activity: &str) -> NewSession {
        let ts = chrono::Utc::now().to_rfc3339();
        NewSession {
            id: id.into(),
            name: "S".into(),
            folder_id: "f1".into(),
            project_id: project_id.map(|p| p.to_string()),
            created_at: ts,
            last_activity: last_activity.into(),
            ..Default::default()
        }
    }

    #[test]
    fn create_then_get_then_update_roundtrip() {
        let db = Db::in_memory().unwrap();
        seed_folder(&db);

        let created = db
            .create_session_blocking(new_session("s1", None, "2026-01-01T00:00:00Z"))
            .unwrap();
        assert_eq!(created.id, "s1");
        assert_eq!(created.name, "S");

        let fetched = db.get_session_blocking("s1").unwrap();
        assert_eq!(fetched.map(|s| s.id), Some("s1".to_string()));
        assert!(db.get_session_blocking("nope").unwrap().is_none());

        let updated = db
            .update_session_blocking(
                "s1",
                UpdateSession {
                    name: Some("renamed".into()),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(updated.name, "renamed");
        // Missing id returns None (use a non-empty changeset; an empty
        // `AsChangeset` is rejected before the row lookup).
        assert!(
            db.update_session_blocking(
                "nope",
                UpdateSession {
                    name: Some("x".into()),
                    ..Default::default()
                },
            )
            .unwrap()
            .is_none()
        );
    }
}
