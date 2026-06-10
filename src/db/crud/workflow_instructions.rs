use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    /// Return every additional-instructions row stored for `project_id`.
    /// Used by the edit-project UI and the worker prompt builder.
    pub async fn list_project_workflow_instructions(
        &self,
        project_id: &str,
    ) -> anyhow::Result<Vec<ProjectWorkflowInstruction>> {
        let project_id = project_id.to_string();
        self.with_conn(move |conn| {
            project_workflow_instructions::table
                .filter(project_workflow_instructions::project_id.eq(&project_id))
                .select(ProjectWorkflowInstruction::as_select())
                .order((
                    project_workflow_instructions::workflow_id.asc(),
                    project_workflow_instructions::step.asc(),
                ))
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Look up the additional-instructions text for a single
    /// (project, workflow, step) combination. Returns `None` when no
    /// override has been set or when the stored text is empty.
    pub async fn get_project_workflow_instruction(
        &self,
        project_id: &str,
        workflow_id: &str,
        step: &str,
    ) -> anyhow::Result<Option<String>> {
        let project_id = project_id.to_string();
        let workflow_id = workflow_id.to_string();
        let step = step.to_string();
        self.with_conn(move |conn| {
            let row: Option<ProjectWorkflowInstruction> = project_workflow_instructions::table
                .find((&project_id, &workflow_id, &step))
                .select(ProjectWorkflowInstruction::as_select())
                .first(conn)
                .optional()?;
            Ok(row.map(|r| r.instructions).filter(|s| !s.trim().is_empty()))
        })
        .await
    }

    /// Upsert a row. An empty-after-trim `instructions` deletes the row
    /// instead of storing whitespace — the absence of a row is the
    /// canonical "no additional instructions" state.
    pub async fn upsert_project_workflow_instruction(
        &self,
        project_id: &str,
        workflow_id: &str,
        step: &str,
        instructions: &str,
    ) -> anyhow::Result<Option<ProjectWorkflowInstruction>> {
        let project_id = project_id.to_string();
        let workflow_id = workflow_id.to_string();
        let step = step.to_string();
        let trimmed = instructions.trim().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        self.with_conn(move |conn| {
            if trimmed.is_empty() {
                diesel::delete(project_workflow_instructions::table.find((
                    &project_id,
                    &workflow_id,
                    &step,
                )))
                .execute(conn)?;
                return Ok(None);
            }
            let row = ProjectWorkflowInstruction {
                project_id: project_id.clone(),
                workflow_id: workflow_id.clone(),
                step: step.clone(),
                instructions: trimmed,
                created_at: now.clone(),
                updated_at: now,
            };
            diesel::insert_into(project_workflow_instructions::table)
                .values(&row)
                .on_conflict((
                    project_workflow_instructions::project_id,
                    project_workflow_instructions::workflow_id,
                    project_workflow_instructions::step,
                ))
                .do_update()
                .set((
                    project_workflow_instructions::instructions.eq(&row.instructions),
                    project_workflow_instructions::updated_at.eq(&row.updated_at),
                ))
                .execute(conn)?;
            Ok(Some(row))
        })
        .await
    }

    /// Drop a single (project, workflow, step) override. Idempotent —
    /// returns `false` if no row existed.
    pub async fn delete_project_workflow_instruction(
        &self,
        project_id: &str,
        workflow_id: &str,
        step: &str,
    ) -> anyhow::Result<bool> {
        let project_id = project_id.to_string();
        let workflow_id = workflow_id.to_string();
        let step = step.to_string();
        self.with_conn(move |conn| {
            let count = diesel::delete(project_workflow_instructions::table.find((
                &project_id,
                &workflow_id,
                &step,
            )))
            .execute(conn)?;
            Ok(count > 0)
        })
        .await
    }
}
