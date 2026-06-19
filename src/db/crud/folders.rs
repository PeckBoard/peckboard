use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    pub async fn create_folder(&self, new: NewFolder) -> anyhow::Result<Folder> {
        self.with_conn(move |conn| {
            diesel::insert_into(folders::table)
                .values(&new)
                .returning(Folder::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_folder(&self, id: &str) -> anyhow::Result<Option<Folder>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            folders::table
                .find(&id)
                .select(Folder::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Synchronous twin of [`Db::get_folder`] for the WASM plugin host
    /// functions (which run on a blocking thread; see `Db::with_conn_blocking`).
    pub(crate) fn get_folder_blocking(&self, id: &str) -> anyhow::Result<Option<Folder>> {
        let id = id.to_string();
        self.with_conn_blocking(move |conn| {
            folders::table
                .find(&id)
                .select(Folder::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
    }

    pub async fn list_folders(&self) -> anyhow::Result<Vec<Folder>> {
        self.with_conn(move |conn| {
            folders::table
                .select(Folder::as_select())
                .order(folders::name.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn delete_folder(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let count = diesel::delete(folders::table.find(&id)).execute(conn)?;
            Ok(count > 0)
        })
        .await
    }

    /// Move a single session into a different folder, atomically. The
    /// session must be a plain (non-worker, non-expert) chat session —
    /// workers and experts inherit their project's folder, so moving one
    /// of them in isolation would silently desync from its project. The
    /// caller is responsible for cancelling any running agent for this
    /// session before invoking; this method only mutates the row. Returns
    /// the updated session, or `None` when the id doesn't exist.
    pub async fn move_session_to_folder(
        &self,
        session_id: &str,
        target_folder_id: &str,
    ) -> anyhow::Result<MoveFolderOutcome<Session>> {
        let session_id = session_id.to_string();
        let target = target_folder_id.to_string();
        self.with_conn(move |conn| {
            let session: Option<Session> = sessions::table
                .find(&session_id)
                .select(Session::as_select())
                .first(conn)
                .optional()?;
            let Some(session) = session else {
                return Ok(MoveFolderOutcome::NotFound);
            };
            if session.is_worker || session.is_expert {
                return Ok(MoveFolderOutcome::RefusedOwnedSession);
            }
            // Target folder must exist; otherwise the FK would 500.
            let folder_exists: bool = folders::table
                .find(&target)
                .select(folders::id)
                .first::<String>(conn)
                .optional()?
                .is_some();
            if !folder_exists {
                return Ok(MoveFolderOutcome::TargetMissing);
            }
            let updated: Session = diesel::update(sessions::table.find(&session_id))
                .set(sessions::folder_id.eq(&target))
                .returning(Session::as_returning())
                .get_result(conn)?;
            // Drop any queued message for this session — the queue can
            // only meaningfully fire in the OLD folder's worker context.
            diesel::delete(queued_messages::table.find(&session_id)).execute(conn)?;
            Ok(MoveFolderOutcome::Moved(updated))
        })
        .await
    }

    /// Move a project to a different folder, atomically, dragging along
    /// every session it owns (workers + experts) so the folder column
    /// stays in lockstep with the project's. The caller cancels running
    /// agents first; this method only mutates rows.
    pub async fn move_project_to_folder(
        &self,
        project_id: &str,
        target_folder_id: &str,
    ) -> anyhow::Result<MoveFolderOutcome<ProjectMoveReport>> {
        let project_id = project_id.to_string();
        let target = target_folder_id.to_string();
        self.with_conn(move |conn| {
            let project: Option<Project> = projects::table
                .find(&project_id)
                .select(Project::as_select())
                .first(conn)
                .optional()?;
            let Some(project) = project else {
                return Ok(MoveFolderOutcome::NotFound);
            };
            let folder_exists: bool = folders::table
                .find(&target)
                .select(folders::id)
                .first::<String>(conn)
                .optional()?
                .is_some();
            if !folder_exists {
                return Ok(MoveFolderOutcome::TargetMissing);
            }
            // Update the project row + every session bound to it. Sessions
            // are dragged along so worker/expert rows don't end up with a
            // folder_id that disagrees with their project.folder_id —
            // that invariant powers every folder-scope check in the MCP
            // tools.
            let owned_session_ids: Vec<String> = sessions::table
                .filter(sessions::project_id.eq(&project_id))
                .select(sessions::id)
                .load(conn)?;
            let sessions_moved =
                diesel::update(sessions::table.filter(sessions::project_id.eq(&project_id)))
                    .set(sessions::folder_id.eq(&target))
                    .execute(conn)?;
            // Wipe queued messages for every moved session in one shot.
            for sid in &owned_session_ids {
                diesel::delete(queued_messages::table.find(sid)).execute(conn)?;
            }
            let updated_project: Project = diesel::update(projects::table.find(&project_id))
                .set(projects::folder_id.eq(&target))
                .returning(Project::as_returning())
                .get_result(conn)?;
            Ok(MoveFolderOutcome::Moved(ProjectMoveReport {
                project: updated_project,
                previous_folder_id: project.folder_id,
                owned_session_ids,
                sessions_moved,
            }))
        })
        .await
    }

    /// Move a repeating task to a different folder, atomically. Sessions
    /// the task previously spawned (`sessions.repeating_task_id = task.id`)
    /// also have their folder column updated so a previously-spawned chat
    /// log stays attached to the same task without acquiring a
    /// stale-folder reference. Worker / expert sessions are NOT moved
    /// here — repeating tasks don't spawn those.
    pub async fn move_repeating_task_to_folder(
        &self,
        task_id: &str,
        target_folder_id: &str,
    ) -> anyhow::Result<MoveFolderOutcome<RepeatingTaskMoveReport>> {
        let task_id = task_id.to_string();
        let target = target_folder_id.to_string();
        self.with_conn(move |conn| {
            let task: Option<RepeatingTask> = repeating_tasks::table
                .find(&task_id)
                .select(RepeatingTask::as_select())
                .first(conn)
                .optional()?;
            let Some(task) = task else {
                return Ok(MoveFolderOutcome::NotFound);
            };
            let folder_exists: bool = folders::table
                .find(&target)
                .select(folders::id)
                .first::<String>(conn)
                .optional()?
                .is_some();
            if !folder_exists {
                return Ok(MoveFolderOutcome::TargetMissing);
            }
            let owned_session_ids: Vec<String> = sessions::table
                .filter(sessions::repeating_task_id.eq(&task_id))
                .select(sessions::id)
                .load(conn)?;
            let sessions_moved =
                diesel::update(sessions::table.filter(sessions::repeating_task_id.eq(&task_id)))
                    .set(sessions::folder_id.eq(&target))
                    .execute(conn)?;
            for sid in &owned_session_ids {
                diesel::delete(queued_messages::table.find(sid)).execute(conn)?;
            }
            let updated_task: RepeatingTask = diesel::update(repeating_tasks::table.find(&task_id))
                .set(repeating_tasks::folder_id.eq(&target))
                .returning(RepeatingTask::as_returning())
                .get_result(conn)?;
            Ok(MoveFolderOutcome::Moved(RepeatingTaskMoveReport {
                task: updated_task,
                previous_folder_id: task.folder_id,
                owned_session_ids,
                sessions_moved,
            }))
        })
        .await
    }
}

/// Outcome of a folder-move call. The DB layer never panics on a bad
/// caller-supplied id; the routes translate each variant to the right
/// HTTP status.
#[derive(Debug)]
pub enum MoveFolderOutcome<T> {
    Moved(T),
    NotFound,
    TargetMissing,
    /// The session is owned by its project (worker / expert); move the
    /// project instead.
    RefusedOwnedSession,
}

#[derive(Debug)]
pub struct ProjectMoveReport {
    pub project: Project,
    pub previous_folder_id: String,
    pub owned_session_ids: Vec<String>,
    pub sessions_moved: usize,
}

#[derive(Debug)]
pub struct RepeatingTaskMoveReport {
    pub task: RepeatingTask,
    pub previous_folder_id: String,
    pub owned_session_ids: Vec<String>,
    pub sessions_moved: usize,
}
