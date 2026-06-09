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

    /// Tasks the scheduler should consider on this tick: enabled and either
    /// never run yet (`next_run_at IS NULL`) or due (`next_run_at <= now`).
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
}
