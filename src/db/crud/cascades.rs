//! Cascade deletes.
//!
//! These methods replace the older "list → loop → delete" patterns that did
//! each step in its own `with_conn` await. The problems they fixed:
//!   * Concurrent writers could add a child (e.g. a worker session) after the
//!     empty-check but before the parent delete, leaving orphaned rows or FK
//!     violations.
//!   * Per-row deletes inside a loop used `let _ = …`, silently swallowing
//!     failures and reporting success even when the cascade was only
//!     half-applied.
//! Each method runs the entire cascade in a single `with_conn` closure, so the
//! process-wide connection mutex serialises it against every other DB
//! operation, and the first error short-circuits the whole batch.
//! `card_dependencies` edges are cleaned by the SQLite `ON DELETE CASCADE`
//! defined on its FKs.

use diesel::prelude::*;

use super::{CascadeReport, FolderEmptyDelete};
use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    /// Delete a folder only if it has no sessions. Atomic: the empty-
    /// check and the delete run while the connection mutex is held, so
    /// a concurrent session creation cannot slip in.
    pub async fn delete_folder_if_empty(&self, id: &str) -> anyhow::Result<FolderEmptyDelete> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let folder_exists: bool = folders::table
                .find(&id)
                .select(folders::id)
                .first::<String>(conn)
                .optional()?
                .is_some();
            if !folder_exists {
                return Ok(FolderEmptyDelete::NotFound);
            }
            let session_count: i64 = sessions::table
                .filter(sessions::folder_id.eq(&id))
                .count()
                .get_result(conn)?;
            if session_count > 0 {
                return Ok(FolderEmptyDelete::HasSessions(session_count as usize));
            }
            diesel::delete(folders::table.find(&id)).execute(conn)?;
            Ok(FolderEmptyDelete::Deleted)
        })
        .await
    }

    /// Delete a folder along with every session it owns plus the
    /// sessions' events and queued messages. Atomic; reports an error
    /// if any step fails instead of silently leaving orphans behind.
    pub async fn delete_folder_cascade(&self, id: &str) -> anyhow::Result<CascadeReport> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let session_ids: Vec<String> = sessions::table
                .filter(sessions::folder_id.eq(&id))
                .select(sessions::id)
                .load(conn)?;

            let mut events_deleted = 0usize;
            for sid in &session_ids {
                events_deleted += diesel::delete(events::table.filter(events::session_id.eq(sid)))
                    .execute(conn)?;
                diesel::delete(queued_messages::table.find(sid)).execute(conn)?;
                diesel::delete(todos::table.filter(todos::session_id.eq(sid))).execute(conn)?;
            }
            let sessions_deleted =
                diesel::delete(sessions::table.filter(sessions::folder_id.eq(&id)))
                    .execute(conn)?;
            let folder_deleted = diesel::delete(folders::table.find(&id)).execute(conn)?;
            if folder_deleted == 0 {
                anyhow::bail!("folder not found: {id}");
            }
            Ok(CascadeReport {
                sessions_deleted,
                events_deleted,
                cards_deleted: 0,
            })
        })
        .await
    }

    /// Delete a project along with every card it owns, every worker
    /// session referenced by those cards, and those sessions' events
    /// and queued messages. Atomic; an early failure aborts the whole
    /// cascade rather than leaving partial state.
    pub async fn delete_project_cascade(&self, id: &str) -> anyhow::Result<CascadeReport> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            // Gather worker session ids from cards before we delete them.
            let cards_in_project: Vec<Card> = cards::table
                .filter(cards::project_id.eq(&id))
                .select(Card::as_select())
                .load(conn)?;
            let mut session_ids: Vec<String> = Vec::new();
            for c in &cards_in_project {
                if let Some(ref sid) = c.worker_session_id {
                    session_ids.push(sid.clone());
                }
                if let Some(ref sid) = c.last_worker_session_id {
                    session_ids.push(sid.clone());
                }
            }
            session_ids.sort();
            session_ids.dedup();

            // Clear card FK references so the session deletes can succeed
            // without FK violations.
            diesel::update(cards::table.filter(cards::project_id.eq(&id)))
                .set((
                    cards::worker_session_id.eq::<Option<String>>(None),
                    cards::last_worker_session_id.eq::<Option<String>>(None),
                ))
                .execute(conn)?;

            let mut events_deleted = 0usize;
            for sid in &session_ids {
                events_deleted += diesel::delete(events::table.filter(events::session_id.eq(sid)))
                    .execute(conn)?;
                diesel::delete(queued_messages::table.find(sid)).execute(conn)?;
                diesel::delete(todos::table.filter(todos::session_id.eq(sid))).execute(conn)?;
            }
            let mut sessions_deleted = 0usize;
            for sid in &session_ids {
                sessions_deleted += diesel::delete(sessions::table.find(sid)).execute(conn)?;
            }
            let cards_deleted =
                diesel::delete(cards::table.filter(cards::project_id.eq(&id))).execute(conn)?;
            let project_deleted = diesel::delete(projects::table.find(&id)).execute(conn)?;
            if project_deleted == 0 {
                anyhow::bail!("project not found: {id}");
            }
            Ok(CascadeReport {
                sessions_deleted,
                events_deleted,
                cards_deleted,
            })
        })
        .await
    }

    /// Delete a card along with every worker session it owns and those
    /// sessions' events and queued messages. Atomic.
    pub async fn delete_card_cascade(&self, id: &str) -> anyhow::Result<CascadeReport> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let card: Option<Card> = cards::table
                .find(&id)
                .select(Card::as_select())
                .first(conn)
                .optional()?;
            let Some(card) = card else {
                anyhow::bail!("card not found: {id}");
            };
            let mut session_ids: Vec<String> = Vec::new();
            if let Some(ref sid) = card.worker_session_id {
                session_ids.push(sid.clone());
            }
            if let Some(ref sid) = card.last_worker_session_id {
                session_ids.push(sid.clone());
            }
            session_ids.sort();
            session_ids.dedup();

            // Clear FK refs first so session delete succeeds.
            diesel::update(cards::table.find(&id))
                .set((
                    cards::worker_session_id.eq::<Option<String>>(None),
                    cards::last_worker_session_id.eq::<Option<String>>(None),
                ))
                .execute(conn)?;

            let mut events_deleted = 0usize;
            for sid in &session_ids {
                events_deleted += diesel::delete(events::table.filter(events::session_id.eq(sid)))
                    .execute(conn)?;
                diesel::delete(queued_messages::table.find(sid)).execute(conn)?;
                diesel::delete(todos::table.filter(todos::session_id.eq(sid))).execute(conn)?;
            }
            let mut sessions_deleted = 0usize;
            for sid in &session_ids {
                sessions_deleted += diesel::delete(sessions::table.find(sid)).execute(conn)?;
            }
            let card_deleted = diesel::delete(cards::table.find(&id)).execute(conn)?;
            if card_deleted == 0 {
                anyhow::bail!("card not found: {id}");
            }
            Ok(CascadeReport {
                sessions_deleted,
                events_deleted,
                cards_deleted: 1,
            })
        })
        .await
    }
}
