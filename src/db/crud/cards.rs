use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

/// Mutate `update.completed_at` so it reflects the step transition
/// implied by `update.step` relative to `prev_step`. Called from both
/// `update_card` and `update_card_atomic` so every step transition —
/// whether it goes through the policy-checking route handler, the MCP
/// tool, or the worker orchestrator — stamps a consistent timestamp
/// without each call site remembering to do it.
fn stamp_completed_at(prev_step: &str, update: &mut UpdateCard) {
    let Some(new_step) = update.step.as_deref() else {
        return;
    };
    if new_step == "done" && prev_step != "done" {
        update.completed_at = Some(Some(chrono::Utc::now().to_rfc3339()));
    } else if prev_step == "done" && new_step != "done" {
        update.completed_at = Some(None);
    }
}

impl Db {
    pub async fn create_card(&self, new: NewCard) -> anyhow::Result<Card> {
        self.with_conn(move |conn| {
            diesel::insert_into(cards::table)
                .values(&new)
                .returning(Card::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_card(&self, id: &str) -> anyhow::Result<Option<Card>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            cards::table
                .find(&id)
                .select(Card::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_cards_by_project(&self, project_id: &str) -> anyhow::Result<Vec<Card>> {
        let project_id = project_id.to_string();
        self.with_conn(move |conn| {
            cards::table
                .filter(cards::project_id.eq(&project_id))
                .select(Card::as_select())
                // priority ASC is the pickup order; created_at ASC as
                // tiebreaker means a brand-new card at the same priority
                // queues behind existing ones rather than jumping ahead.
                .order((cards::priority.asc(), cards::created_at.asc()))
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn update_card(&self, id: &str, update: UpdateCard) -> anyhow::Result<Option<Card>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let mut update = update;
            // Stamp completed_at on transitions into/out of the `done`
            // step so the Kanban "Done" column can sort by most-recently
            // finished. We read the existing row inside the same
            // connection scope, so the read/write pair is atomic under
            // the DB connection mutex — concurrent step changes can't
            // interleave between the lookup and the write.
            if update.step.is_some() && update.completed_at.is_none() {
                let existing: Option<String> = cards::table
                    .find(&id)
                    .select(cards::step)
                    .first::<String>(conn)
                    .optional()?;
                if let Some(prev_step) = existing {
                    stamp_completed_at(&prev_step, &mut update);
                }
            }
            diesel::update(cards::table.find(&id))
                .set(&update)
                .returning(Card::as_returning())
                .get_result(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Atomic read-validate-update for a card. The closure runs while the
    /// process-wide DB connection mutex is held, so concurrent transitions
    /// cannot interleave between the read and the write. Returns:
    /// * `Ok(Some(card))` on a successful update,
    /// * `Ok(None)` if the card no longer exists,
    /// * `Err(...)` if the closure rejected the transition (the caller can
    ///   map this to a 4xx response) or a DB error occurred.
    ///
    /// This is the right primitive for transitions whose validity depends
    /// on the current row state (terminal-state guards, step transitions,
    /// "only step changes allowed", etc.). The non-atomic alternative —
    /// `get_card().await; check; update_card().await` — has a race window
    /// between the two awaits where another writer can change the state.
    pub async fn update_card_atomic<F>(&self, id: &str, validate: F) -> anyhow::Result<Option<Card>>
    where
        F: FnOnce(&Card) -> anyhow::Result<UpdateCard> + Send + 'static,
    {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let existing: Option<Card> = cards::table
                .find(&id)
                .select(Card::as_select())
                .first(conn)
                .optional()?;
            let Some(existing) = existing else {
                return Ok(None);
            };
            let mut update = validate(&existing)?;
            if update.step.is_some() && update.completed_at.is_none() {
                stamp_completed_at(&existing.step, &mut update);
            }
            diesel::update(cards::table.find(&id))
                .set(&update)
                .returning(Card::as_returning())
                .get_result(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn delete_cards_by_project(&self, project_id: &str) -> anyhow::Result<usize> {
        let project_id = project_id.to_string();
        self.with_conn(move |conn| {
            diesel::delete(cards::table.filter(cards::project_id.eq(&project_id)))
                .execute(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn delete_card(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let count = diesel::delete(cards::table.find(&id)).execute(conn)?;
            Ok(count > 0)
        })
        .await
    }
}
