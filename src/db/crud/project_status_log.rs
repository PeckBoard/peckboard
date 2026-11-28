//! Who paused or resumed a project, and when (`project_status_log`).
//!
//! A project's status can be flipped by a person in the UI, by any agent
//! session through the MCP tools, or by the server itself. The row on
//! `projects` only says what the status is now; this log says who made it
//! so, which is what the board shows next to "paused".

use diesel::prelude::*;
use diesel::sql_types::{BigInt, Nullable, Text};

use crate::db::Db;

/// One pause or resume.
#[derive(QueryableByName, Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProjectStatusEntry {
    #[diesel(sql_type = Text)]
    pub id: String,
    #[diesel(sql_type = Text)]
    pub project_id: String,
    /// `paused` or `resumed`.
    #[diesel(sql_type = Text)]
    pub action: String,
    /// `user`, `session` (an agent) or `system`.
    #[diesel(sql_type = Text)]
    pub actor_kind: String,
    /// The user id or session id; `None` for the system.
    #[diesel(sql_type = Nullable<Text>)]
    pub actor_id: Option<String>,
    /// The username or session name as it was at the time.
    #[diesel(sql_type = Text)]
    pub actor_name: String,
    /// RFC 3339, UTC.
    #[diesel(sql_type = Text)]
    pub at: String,
}

const COLUMNS: &str = "id, project_id, action, actor_kind, actor_id, actor_name, at";

impl Db {
    /// Append one entry. `action` is `paused` or `resumed`.
    pub async fn log_project_status(
        &self,
        project_id: &str,
        action: &'static str,
        actor_kind: &str,
        actor_id: Option<&str>,
        actor_name: &str,
    ) -> anyhow::Result<()> {
        let id = uuid::Uuid::new_v4().to_string();
        let project_id = project_id.to_string();
        let actor_kind = actor_kind.to_string();
        let actor_id = actor_id.map(str::to_string);
        let actor_name = actor_name.to_string();
        let at = chrono::Utc::now().to_rfc3339();
        self.with_conn(move |conn| {
            diesel::sql_query(
                "INSERT INTO project_status_log \
                 (id, project_id, action, actor_kind, actor_id, actor_name, at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .bind::<Text, _>(id)
            .bind::<Text, _>(project_id)
            .bind::<Text, _>(action)
            .bind::<Text, _>(actor_kind)
            .bind::<Nullable<Text>, _>(actor_id)
            .bind::<Text, _>(actor_name)
            .bind::<Text, _>(at)
            .execute(conn)?;
            Ok(())
        })
        .await
    }

    /// The newest `limit` entries of a project, newest first.
    pub async fn list_project_status(
        &self,
        project_id: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<ProjectStatusEntry>> {
        let project_id = project_id.to_string();
        self.read(move |conn| {
            diesel::sql_query(format!(
                "SELECT {COLUMNS} FROM project_status_log \
                 WHERE project_id = ?1 ORDER BY at DESC, rowid DESC LIMIT ?2"
            ))
            .bind::<Text, _>(project_id)
            .bind::<BigInt, _>(limit)
            .load(conn)
            .map_err(Into::into)
        })
        .await
    }

    /// The latest pause or resume of a project, if any was recorded.
    pub async fn last_project_status(
        &self,
        project_id: &str,
    ) -> anyhow::Result<Option<ProjectStatusEntry>> {
        Ok(self
            .list_project_status(project_id, 1)
            .await?
            .into_iter()
            .next())
    }
}
