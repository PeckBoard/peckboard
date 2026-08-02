use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

/// A custom workflow with its ordered steps, as stored.
#[derive(Debug, Clone)]
pub struct CustomWorkflowWithSteps {
    pub row: CustomWorkflowRow,
    pub steps: Vec<CustomWorkflowStepRow>,
}

/// A project referencing a custom workflow, surfaced on a blocked delete.
#[derive(Debug, Clone)]
pub struct WorkflowReference {
    pub project_id: String,
    pub project_name: String,
}

fn load_steps(
    conn: &mut SqliteConnection,
    workflow_id: &str,
) -> anyhow::Result<Vec<CustomWorkflowStepRow>> {
    custom_workflow_steps::table
        .filter(custom_workflow_steps::workflow_id.eq(workflow_id))
        .select(CustomWorkflowStepRow::as_select())
        .order(custom_workflow_steps::position.asc())
        .load(conn)
        .map_err(Into::into)
}

impl Db {
    /// Every custom workflow, each paired with its ordered steps.
    pub async fn list_custom_workflows(&self) -> anyhow::Result<Vec<CustomWorkflowWithSteps>> {
        self.with_conn(move |conn| {
            let rows: Vec<CustomWorkflowRow> = custom_workflows::table
                .select(CustomWorkflowRow::as_select())
                .order(custom_workflows::name.asc())
                .load(conn)?;
            rows.into_iter()
                .map(|row| {
                    let steps = load_steps(conn, &row.id)?;
                    Ok(CustomWorkflowWithSteps { row, steps })
                })
                .collect()
        })
        .await
    }

    /// Single custom workflow by id, with its ordered steps.
    pub async fn get_custom_workflow(
        &self,
        id: &str,
    ) -> anyhow::Result<Option<CustomWorkflowWithSteps>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let row: Option<CustomWorkflowRow> = custom_workflows::table
                .find(&id)
                .select(CustomWorkflowRow::as_select())
                .first(conn)
                .optional()?;
            match row {
                Some(row) => {
                    let steps = load_steps(conn, &row.id)?;
                    Ok(Some(CustomWorkflowWithSteps { row, steps }))
                }
                None => Ok(None),
            }
        })
        .await
    }

    /// Create a custom workflow and its steps in one transaction. Caller
    /// has already validated the shape and id/name uniqueness against the
    /// merged (built-in + custom) registry.
    pub async fn create_custom_workflow(
        &self,
        row: CustomWorkflowRow,
        steps: Vec<CustomWorkflowStepRow>,
    ) -> anyhow::Result<CustomWorkflowWithSteps> {
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                diesel::insert_into(custom_workflows::table)
                    .values(&row)
                    .execute(conn)?;
                if !steps.is_empty() {
                    diesel::insert_into(custom_workflow_steps::table)
                        .values(&steps)
                        .execute(conn)?;
                }
                Ok(())
            })?;
            Ok(CustomWorkflowWithSteps { row, steps })
        })
        .await
    }

    /// Replace a custom workflow's metadata and full step list. Steps are
    /// deleted and reinserted rather than diffed — the step count is small
    /// and this keeps the (workflow_id, step) uniqueness trivially correct.
    pub async fn update_custom_workflow(
        &self,
        id: &str,
        name: String,
        description: String,
        priority: i32,
        steps: Vec<CustomWorkflowStepRow>,
        updated_at: String,
    ) -> anyhow::Result<Option<CustomWorkflowWithSteps>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let exists = custom_workflows::table
                .find(&id)
                .select(custom_workflows::id)
                .first::<String>(conn)
                .optional()?;
            if exists.is_none() {
                return Ok(None);
            }
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                diesel::update(custom_workflows::table.find(&id))
                    .set((
                        custom_workflows::name.eq(&name),
                        custom_workflows::description.eq(&description),
                        custom_workflows::priority.eq(priority),
                        custom_workflows::updated_at.eq(&updated_at),
                    ))
                    .execute(conn)?;
                diesel::delete(
                    custom_workflow_steps::table.filter(custom_workflow_steps::workflow_id.eq(&id)),
                )
                .execute(conn)?;
                if !steps.is_empty() {
                    diesel::insert_into(custom_workflow_steps::table)
                        .values(&steps)
                        .execute(conn)?;
                }
                Ok(())
            })?;
            let row: CustomWorkflowRow = custom_workflows::table
                .find(&id)
                .select(CustomWorkflowRow::as_select())
                .first(conn)?;
            let steps = load_steps(conn, &id)?;
            Ok(Some(CustomWorkflowWithSteps { row, steps }))
        })
        .await
    }

    /// Delete a custom workflow (its steps cascade via the FK). Idempotent
    /// — returns `false` if it didn't exist. Callers MUST check
    /// `custom_workflow_references` first; this does no reference check
    /// itself.
    pub async fn delete_custom_workflow(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let count = diesel::delete(custom_workflows::table.find(&id)).execute(conn)?;
            Ok(count > 0)
        })
        .await
    }

    /// Projects whose `workflow` column names this workflow id, plus the
    /// count of cards that name it directly (a card's workflow is copied
    /// at create time and can diverge from its project's). Used to block
    /// deleting a workflow that's still in use, with enough detail for a
    /// clear 409 message.
    pub async fn custom_workflow_references(
        &self,
        workflow_id: &str,
    ) -> anyhow::Result<(Vec<WorkflowReference>, i64)> {
        let workflow_id = workflow_id.to_string();
        self.with_conn(move |conn| {
            let projects: Vec<(String, String)> = projects::table
                .filter(projects::workflow.eq(&workflow_id))
                .select((projects::id, projects::name))
                .load(conn)?;
            let card_count: i64 = cards::table
                .filter(cards::workflow.eq(&workflow_id))
                .count()
                .get_result(conn)?;
            Ok((
                projects
                    .into_iter()
                    .map(|(project_id, project_name)| WorkflowReference {
                        project_id,
                        project_name,
                    })
                    .collect(),
                card_count,
            ))
        })
        .await
    }
}
