use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    pub async fn create_repeating_task(
        &self,
        new: NewRepeatingTask,
    ) -> anyhow::Result<RepeatingTask> {
        self.with_conn(move |conn| {
            diesel::insert_into(repeating_tasks::table)
                .values(&new)
                .returning(RepeatingTask::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_repeating_task(&self, id: &str) -> anyhow::Result<Option<RepeatingTask>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            repeating_tasks::table
                .find(&id)
                .select(RepeatingTask::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_repeating_tasks(&self) -> anyhow::Result<Vec<RepeatingTask>> {
        self.with_conn(move |conn| {
            repeating_tasks::table
                .select(RepeatingTask::as_select())
                .order(repeating_tasks::name.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_repeating_tasks_by_folder(
        &self,
        folder_id: &str,
    ) -> anyhow::Result<Vec<RepeatingTask>> {
        let folder_id = folder_id.to_string();
        self.with_conn(move |conn| {
            repeating_tasks::table
                .filter(repeating_tasks::folder_id.eq(&folder_id))
                .select(RepeatingTask::as_select())
                .order(repeating_tasks::name.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Enabled tasks. The scheduler tick loads this set and decides
    /// due-ness in Rust via [`crate::repeating::is_scheduler_due`]
    /// (last execution vs the next scheduled slot), not by filtering
    /// on the stored `next_run_at` column.
    pub async fn list_enabled_repeating_tasks(&self) -> anyhow::Result<Vec<RepeatingTask>> {
        self.with_conn(move |conn| {
            repeating_tasks::table
                .filter(repeating_tasks::enabled.eq(true))
                .select(RepeatingTask::as_select())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Tasks whose stored `next_run_at` is NULL or `<= now`. Kept for
    /// the column-filter test and as a UI hint query — the scheduler
    /// trigger itself uses [`list_enabled_repeating_tasks`] plus
    /// [`crate::repeating::is_scheduler_due`].
    pub async fn list_due_repeating_tasks(
        &self,
        now_rfc3339: &str,
    ) -> anyhow::Result<Vec<RepeatingTask>> {
        let now = now_rfc3339.to_string();
        self.with_conn(move |conn| {
            repeating_tasks::table
                .filter(repeating_tasks::enabled.eq(true))
                .filter(
                    repeating_tasks::next_run_at
                        .is_null()
                        .or(repeating_tasks::next_run_at.le(&now)),
                )
                .select(RepeatingTask::as_select())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn update_repeating_task(
        &self,
        id: &str,
        update: UpdateRepeatingTask,
    ) -> anyhow::Result<Option<RepeatingTask>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            diesel::update(repeating_tasks::table.find(&id))
                .set(&update)
                .returning(RepeatingTask::as_returning())
                .get_result(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn delete_repeating_task(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            // Detach (but don't delete) the sessions that this task spawned
            // so the user can still browse the run history if they wish.
            // Cascade-delete is intentionally NOT applied: a user typically
            // wants to delete the schedule without losing prior work.
            diesel::update(sessions::table.filter(sessions::repeating_task_id.eq(&id)))
                .set(sessions::repeating_task_id.eq::<Option<String>>(None))
                .execute(conn)?;
            // Run-history rows carry an FK to the task; delete them first.
            diesel::delete(repeating_task_runs::table.filter(repeating_task_runs::task_id.eq(&id)))
                .execute(conn)?;
            // Drop any user_tabs entries pointing at this task — mirrors
            // delete_session/delete_project. user_tabs is polymorphic
            // (no FK cascade), so this is the only path that prevents
            // orphan chips from rendering after a delete.
            diesel::delete(
                user_tabs::table
                    .filter(user_tabs::item_type.eq("repeating_task"))
                    .filter(user_tabs::item_id.eq(&id)),
            )
            .execute(conn)?;
            let count = diesel::delete(repeating_tasks::table.find(&id)).execute(conn)?;
            Ok(count > 0)
        })
        .await
    }

    pub async fn list_sessions_by_repeating_task(
        &self,
        task_id: &str,
    ) -> anyhow::Result<Vec<Session>> {
        let task_id = task_id.to_string();
        self.with_conn(move |conn| {
            sessions::table
                .filter(sessions::repeating_task_id.eq(&task_id))
                .select(Session::as_select())
                .order(sessions::created_at.desc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Record the dispatch outcome of a scheduler tick or manual "Run
    /// now" -- not the eventual agent result, which is already visible
    /// via the spawned session's own event stream. Prunes to the newest
    /// `keep` rows for the task so a fast interval schedule can't grow
    /// this table unbounded.
    pub async fn create_repeating_task_run(
        &self,
        new: NewRepeatingTaskRun,
        keep: i64,
    ) -> anyhow::Result<RepeatingTaskRun> {
        let task_id = new.task_id.clone();
        let row = self
            .with_conn(move |conn| {
                diesel::insert_into(repeating_task_runs::table)
                    .values(&new)
                    .returning(RepeatingTaskRun::as_returning())
                    .get_result(conn)
                    .map_err(anyhow::Error::from)
            })
            .await?;
        self.with_conn(move |conn| {
            let keep_ids: Vec<String> = repeating_task_runs::table
                .filter(repeating_task_runs::task_id.eq(&task_id))
                .select(repeating_task_runs::id)
                .order(repeating_task_runs::started_at.desc())
                .limit(keep)
                .load(conn)?;
            diesel::delete(
                repeating_task_runs::table
                    .filter(repeating_task_runs::task_id.eq(&task_id))
                    .filter(repeating_task_runs::id.ne_all(&keep_ids)),
            )
            .execute(conn)?;
            Ok(())
        })
        .await?;
        Ok(row)
    }

    pub async fn list_repeating_task_runs(
        &self,
        task_id: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<RepeatingTaskRun>> {
        let task_id = task_id.to_string();
        self.with_conn(move |conn| {
            repeating_task_runs::table
                .filter(repeating_task_runs::task_id.eq(&task_id))
                .select(RepeatingTaskRun::as_select())
                .order(repeating_task_runs::started_at.desc())
                .limit(limit)
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }
}
