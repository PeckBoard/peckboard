use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

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
                .order(cards::priority.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn update_card(&self, id: &str, update: UpdateCard) -> anyhow::Result<Option<Card>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
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
            let update = validate(&existing)?;
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
