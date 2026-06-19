use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

/// List all projects, most-recently-accessed first. Free function over a
/// raw connection so it can be shared by the async [`Db::list_projects`] and
/// the synchronous [`Db::list_projects_blocking`] (used by plugin host fns).
pub(crate) fn list_projects_query(conn: &mut SqliteConnection) -> anyhow::Result<Vec<Project>> {
    projects::table
        .select(Project::as_select())
        .order(projects::last_accessed_at.desc())
        .load(conn)
        .map_err(Into::into)
}

/// Fetch a single project by id. Shared by the async [`Db::get_project`] and
/// the synchronous [`Db::get_project_blocking`] (used by plugin host fns).
pub(crate) fn get_project_query(
    conn: &mut SqliteConnection,
    id: &str,
) -> anyhow::Result<Option<Project>> {
    projects::table
        .find(id)
        .select(Project::as_select())
        .first(conn)
        .optional()
        .map_err(Into::into)
}

impl Db {
    pub async fn create_project(&self, new: NewProject) -> anyhow::Result<Project> {
        self.with_conn(move |conn| {
            diesel::insert_into(projects::table)
                .values(&new)
                .returning(Project::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_project(&self, id: &str) -> anyhow::Result<Option<Project>> {
        let id = id.to_string();
        self.with_conn(move |conn| get_project_query(conn, &id))
            .await
    }

    /// Synchronous twin of [`Db::get_project`] for plugin host functions.
    pub(crate) fn get_project_blocking(&self, id: &str) -> anyhow::Result<Option<Project>> {
        self.with_conn_blocking(|conn| get_project_query(conn, id))
    }

    pub async fn list_projects(&self) -> anyhow::Result<Vec<Project>> {
        self.with_conn(list_projects_query).await
    }

    /// Synchronous twin of [`Db::list_projects`] for plugin host functions.
    pub(crate) fn list_projects_blocking(&self) -> anyhow::Result<Vec<Project>> {
        self.with_conn_blocking(list_projects_query)
    }

    pub async fn list_projects_by_folder(&self, folder_id: &str) -> anyhow::Result<Vec<Project>> {
        let folder_id = folder_id.to_string();
        self.with_conn(move |conn| {
            projects::table
                .filter(projects::folder_id.eq(&folder_id))
                .select(Project::as_select())
                .order(projects::last_accessed_at.desc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn update_project(
        &self,
        id: &str,
        update: UpdateProject,
    ) -> anyhow::Result<Option<Project>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            diesel::update(projects::table.find(&id))
                .set(&update)
                .returning(Project::as_returning())
                .get_result(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn delete_project(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            // Mirror delete_session: clean up orphan tab rows so the
            // frontend strip doesn't keep showing a "Project" chip.
            diesel::delete(
                user_tabs::table
                    .filter(user_tabs::item_type.eq("project"))
                    .filter(user_tabs::item_id.eq(&id)),
            )
            .execute(conn)?;
            let count = diesel::delete(projects::table.find(&id)).execute(conn)?;
            Ok(count > 0)
        })
        .await
    }
}
