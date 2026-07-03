use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

// The card list/create query bodies are free functions over a raw connection
// so they can be shared by the async `Db` methods and their synchronous twins
// (`*_blocking`) used by the WASM plugin host functions in `src/plugin/host.rs`.
// `priority ASC, created_at ASC` is the canonical pickup order.

pub(crate) fn list_cards_by_project_query(
    conn: &mut SqliteConnection,
    project_id: &str,
) -> anyhow::Result<Vec<Card>> {
    cards::table
        .filter(cards::project_id.eq(project_id))
        .select(Card::as_select())
        .order((cards::priority.asc(), cards::created_at.asc()))
        .load(conn)
        .map_err(Into::into)
}

pub(crate) fn list_all_cards_query(conn: &mut SqliteConnection) -> anyhow::Result<Vec<Card>> {
    cards::table
        .select(Card::as_select())
        .order((cards::priority.asc(), cards::created_at.asc()))
        .load(conn)
        .map_err(Into::into)
}

pub(crate) fn create_card_query(
    conn: &mut SqliteConnection,
    new: &NewCard,
) -> anyhow::Result<Card> {
    diesel::insert_into(cards::table)
        .values(new)
        .returning(Card::as_returning())
        .get_result(conn)
        .map_err(Into::into)
}

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
/// Sever the resume link between a card and its last worker session when
/// the card's step moves somewhere that session wasn't working.
///
/// A worker session persists for its card while the card stays on the step
/// the session was working, or detours through `backlog` / `wont_do` (being
/// blocked is a flag, not a step, so it never lands here) — coming back to
/// the worked step resumes the same session/conversation instead of
/// spawning a fresh agent (see `spawn_worker_for_card`). Moving the card to
/// any OTHER real step ends that: we null the session's `worker_step` so it
/// is never picked as a resume candidate again. Lives in the DB write path
/// (like `stamp_completed_at`) so every step transition — route handler,
/// MCP tool, orchestrator — enforces the same rule.
fn sever_worker_resume_link(
    conn: &mut SqliteConnection,
    prev_step: &str,
    card: &Card,
) -> anyhow::Result<()> {
    if card.step == prev_step
        || card.step == "backlog"
        || card.step == "todo"
        || card.step == "wont_do"
    {
        return Ok(());
    }
    let Some(sid) = card.last_worker_session_id.as_deref() else {
        return Ok(());
    };
    diesel::update(
        sessions::table
            .find(sid)
            .filter(sessions::worker_step.is_not_null())
            .filter(sessions::worker_step.ne(&card.step)),
    )
    .set(sessions::worker_step.eq::<Option<String>>(None))
    .execute(conn)?;
    Ok(())
}

impl Db {
    pub async fn create_card(&self, new: NewCard) -> anyhow::Result<Card> {
        self.with_conn(move |conn| create_card_query(conn, &new))
            .await
    }

    /// Synchronous twin of [`Db::create_card`] for plugin host functions.
    pub(crate) fn create_card_blocking(&self, new: &NewCard) -> anyhow::Result<Card> {
        self.with_conn_blocking(|conn| create_card_query(conn, new))
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
        self.with_conn(move |conn| list_cards_by_project_query(conn, &project_id))
            .await
    }

    /// Synchronous twins of the card listers for plugin host functions.
    /// `project_id = None` lists cards across all projects.
    pub(crate) fn list_cards_blocking(
        &self,
        project_id: Option<&str>,
    ) -> anyhow::Result<Vec<Card>> {
        match project_id {
            Some(pid) => self.with_conn_blocking(|conn| list_cards_by_project_query(conn, pid)),
            None => self.with_conn_blocking(list_all_cards_query),
        }
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
            let mut prev_step: Option<String> = None;
            if update.step.is_some() {
                prev_step = cards::table
                    .find(&id)
                    .select(cards::step)
                    .first::<String>(conn)
                    .optional()?;
                if let Some(ref prev) = prev_step
                    && update.completed_at.is_none()
                {
                    stamp_completed_at(prev, &mut update);
                }
            }
            let card: Option<Card> = diesel::update(cards::table.find(&id))
                .set(&update)
                .returning(Card::as_returning())
                .get_result(conn)
                .optional()?;
            if let (Some(card), Some(prev)) = (card.as_ref(), prev_step.as_deref()) {
                sever_worker_resume_link(conn, prev, card)?;
            }
            Ok(card)
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
            let card: Option<Card> = diesel::update(cards::table.find(&id))
                .set(&update)
                .returning(Card::as_returning())
                .get_result(conn)
                .optional()?;
            if let Some(card) = card.as_ref() {
                sever_worker_resume_link(conn, &existing.step, card)?;
            }
            Ok(card)
        })
        .await
    }

    /// Clear `cards.worker_session_id` ONLY if it currently equals the
    /// supplied `session_id`, and stamp `last_worker_session_id` so the
    /// auto-pause counter and the card-history view can still join through
    /// this session's events. Returns the updated card on success, `None`
    /// if the card's worker ref no longer matches (already cleared, or
    /// reassigned to a different worker) — in which case the caller MUST
    /// NOT treat its action as having freed a slot.
    ///
    /// Race this guards against: the orchestrator can spawn a replacement
    /// worker on the same card between an outgoing cancel and its
    /// completion listener firing. An unconditional clear in the listener
    /// would wipe the replacement's `worker_session_id`, freeing the slot
    /// a second time and producing two concurrent workers for one card.
    pub async fn clear_card_worker_if_matches(
        &self,
        card_id: &str,
        session_id: &str,
    ) -> anyhow::Result<Option<Card>> {
        let card_id = card_id.to_string();
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            let n = diesel::update(
                cards::table
                    .find(&card_id)
                    .filter(cards::worker_session_id.eq(&session_id)),
            )
            .set((
                cards::worker_session_id.eq::<Option<String>>(None),
                cards::last_worker_session_id.eq(Some(&session_id)),
                cards::updated_at.eq(chrono::Utc::now().to_rfc3339()),
            ))
            .execute(conn)?;
            if n == 0 {
                return Ok(None);
            }
            cards::table
                .find(&card_id)
                .select(Card::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Atomically claim an unassigned card for a worker session. The
    /// conditional `WHERE worker_session_id IS NULL` makes the claim the
    /// single source of truth for "who owns this card": two concurrent
    /// spawn paths (orchestrator ticks, the completion listener, a manual
    /// restart) can both decide to pick the card up, but only one claim
    /// lands — the loser gets `false` and must skip its spawn.
    pub async fn claim_card_for_worker(
        &self,
        id: &str,
        session_id: &str,
        new_step: Option<String>,
        now: &str,
    ) -> anyhow::Result<bool> {
        let id = id.to_string();
        let session_id = session_id.to_string();
        let now = now.to_string();
        self.with_conn(move |conn| {
            let update = UpdateCard {
                worker_session_id: Some(Some(session_id.clone())),
                last_worker_session_id: Some(Some(session_id)),
                step: new_step,
                updated_at: Some(now),
                ..Default::default()
            };
            let count = diesel::update(
                cards::table
                    .find(&id)
                    .filter(cards::worker_session_id.is_null()),
            )
            .set(&update)
            .execute(conn)?;
            Ok(count > 0)
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
