use diesel::connection::Connection;
use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::{Card, NewTodoRow, TodoRow};
use crate::db::schema::{cards, todos};
use crate::todo::{TodoItem, TodoSnapshot, TodoStatus};

/// One card's todos for the project-aggregate view. `card_title` is denormalized
/// so the frontend can render the group label without a second round-trip.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProjectCardTodos {
    pub card_id: String,
    pub card_title: String,
    pub todos: Vec<TodoItem>,
}

impl Db {
    /// Replace the todos for `session_id` with the snapshot's items, in order.
    ///
    /// `TodoWrite` (and every provider's equivalent) is replace-all, so we
    /// wipe the session's existing rows and re-insert. Done in a single
    /// transaction so a reader that races with the write never sees a half-
    /// applied snapshot.
    pub async fn replace_session_todos(
        &self,
        session_id: &str,
        snapshot: TodoSnapshot,
    ) -> anyhow::Result<()> {
        let session_id = session_id.to_string();
        let updated_at = chrono::Utc::now().to_rfc3339();
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                diesel::delete(todos::table.filter(todos::session_id.eq(&session_id)))
                    .execute(conn)?;

                let rows: Vec<NewTodoRow> = snapshot
                    .todos
                    .into_iter()
                    .enumerate()
                    .map(|(idx, item)| NewTodoRow {
                        session_id: session_id.clone(),
                        position: idx as i32,
                        content: item.content,
                        status: status_token(item.status).to_string(),
                        active_form: item.active_form,
                        updated_at: updated_at.clone(),
                    })
                    .collect();

                if !rows.is_empty() {
                    diesel::insert_into(todos::table)
                        .values(&rows)
                        .execute(conn)?;
                }
                Ok(())
            })
        })
        .await
    }

    /// Aggregate the latest todos across every card in `project_id`, grouped by
    /// card. For each card we read the same session id the frontend's
    /// `useProjectTodos` does: the active `worker_session_id` if set, falling
    /// back to `last_worker_session_id` so a snapshot survives the worker
    /// completing a chunk. Cards with no session (and cards whose session never
    /// reported any todos) are omitted, matching the frontend's "uncluttered"
    /// behavior — the view layer is responsible for rendering the empty state.
    /// Output is ordered by card priority (ascending), matching
    /// `list_cards_by_project`.
    pub async fn list_project_todos(
        &self,
        project_id: &str,
    ) -> anyhow::Result<Vec<ProjectCardTodos>> {
        let project_id = project_id.to_string();
        let card_rows: Vec<Card> = self
            .with_conn(move |conn| {
                cards::table
                    .filter(cards::project_id.eq(&project_id))
                    .select(Card::as_select())
                    .order(cards::priority.asc())
                    .load(conn)
                    .map_err(Into::into)
            })
            .await?;

        let mut out = Vec::new();
        for card in card_rows {
            let Some(sid) = card
                .worker_session_id
                .clone()
                .or_else(|| card.last_worker_session_id.clone())
            else {
                continue;
            };
            let todos = self.list_session_todos(&sid).await?;
            if todos.is_empty() {
                continue;
            }
            out.push(ProjectCardTodos {
                card_id: card.id,
                card_title: card.title,
                todos,
            });
        }
        Ok(out)
    }

    /// Load the current todo list for `session_id` in position order.
    pub async fn list_session_todos(&self, session_id: &str) -> anyhow::Result<Vec<TodoItem>> {
        let session_id = session_id.to_string();
        let rows: Vec<TodoRow> = self
            .with_conn(move |conn| {
                todos::table
                    .filter(todos::session_id.eq(&session_id))
                    .select(TodoRow::as_select())
                    .order(todos::position.asc())
                    .load(conn)
                    .map_err(Into::into)
            })
            .await?;
        Ok(rows.into_iter().map(row_to_item).collect())
    }
}

/// Map the canonical lifecycle enum to its on-the-wire snake_case token, the
/// same string used in `TodoStatus`'s `serde(rename_all = "snake_case")` form.
fn status_token(status: TodoStatus) -> &'static str {
    match status {
        TodoStatus::Pending => "pending",
        TodoStatus::InProgress => "in_progress",
        TodoStatus::Done => "done",
    }
}

fn row_to_item(row: TodoRow) -> TodoItem {
    TodoItem {
        content: row.content,
        // Reuse the provider-normalization path: `done` round-trips, and
        // any drift from a bad write still degrades to Pending rather than
        // dropping the row.
        status: TodoStatus::from_provider(&row.status),
        active_form: row.active_form,
    }
}
