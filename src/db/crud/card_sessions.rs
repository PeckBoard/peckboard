//! Card run history (`card_sessions`) and session sealing.
//!
//! Every worker claim on a card writes one row; the row is closed with an
//! outcome + summary when that run ends. Rows outlive the session they name
//! (no FK on `session_id`) and go only with their card (see the cascades).

use diesel::prelude::*;
use serde::Serialize;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

/// What a new run on a card is about to do — recorded on its row.
#[derive(Debug, Clone)]
pub struct CardRunStart {
    pub step: String,
    /// `work` | `review`.
    pub role: String,
    pub model: Option<String>,
}

/// A `card_sessions` row joined with the live session it names, as served by
/// `GET /api/projects/{id}/cards/{card_id}/sessions`.
#[derive(Debug, Clone, Serialize)]
pub struct CardSessionEntry {
    pub id: String,
    pub card_id: String,
    pub session_id: String,
    pub session_name: Option<String>,
    pub session_exists: bool,
    pub sealed: bool,
    pub step: String,
    pub role: String,
    pub model: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub outcome: Option<String>,
    pub summary: Option<String>,
}

/// Result of [`Db::seal_session`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealOutcome {
    /// Newly sealed; `dropped_queued` pending queued messages were removed.
    Sealed {
        dropped_queued: usize,
    },
    AlreadySealed,
    NotFound,
}

/// `sessions.sealed_reason` for a session whose last run ended with
/// `outcome`: `advanced` (its work moved the card on), `reviewed` (its
/// review finished), `moved` (the user moved the card), `stopped`,
/// `wont_do`; anything else — a crash, a run cut short — `superseded`.
pub fn seal_reason_for_outcome(outcome: Option<&str>) -> &'static str {
    match outcome {
        Some("advanced" | "finished") => "advanced",
        Some("reviewed" | "changes_requested") => "reviewed",
        Some("moved") => "moved",
        Some("stopped") => "stopped",
        Some("wont_do") => "wont_do",
        _ => "superseded",
    }
}

/// Close the open runs on `card_id` — only `session_id`'s when given — with
/// `outcome`. `summary = None` keeps whatever summary the row already has.
fn close_runs(
    conn: &mut SqliteConnection,
    card_id: &str,
    session_id: Option<&str>,
    outcome: &str,
    summary: Option<&str>,
    now: &str,
) -> anyhow::Result<usize> {
    use diesel::sql_types::{Nullable, Text};
    let n = diesel::sql_query(
        "UPDATE card_sessions \
            SET ended_at = ?, outcome = ?, summary = COALESCE(?, summary) \
          WHERE card_id = ? AND ended_at IS NULL AND (? IS NULL OR session_id = ?)",
    )
    .bind::<Text, _>(now)
    .bind::<Text, _>(outcome)
    .bind::<Nullable<Text>, _>(summary)
    .bind::<Text, _>(card_id)
    .bind::<Nullable<Text>, _>(session_id)
    .bind::<Nullable<Text>, _>(session_id)
    .execute(conn)?;
    Ok(n)
}

impl Db {
    /// Record a new run of `session_id` on `card_id`. Any run still open on
    /// the card (a run whose end was never observed — restart, lost
    /// completion) is closed as `superseded` first, so at most one run per
    /// card is ever open.
    pub async fn open_card_run(
        &self,
        card_id: &str,
        session_id: &str,
        run: CardRunStart,
        now: &str,
    ) -> anyhow::Result<CardSession> {
        let row = CardSession {
            id: uuid::Uuid::new_v4().to_string(),
            card_id: card_id.to_string(),
            session_id: session_id.to_string(),
            step: run.step,
            role: run.role,
            model: run.model,
            started_at: now.to_string(),
            ended_at: None,
            outcome: None,
            summary: None,
        };
        let now = now.to_string();
        self.with_conn(move |conn| {
            conn.transaction(|conn| {
                close_runs(conn, &row.card_id, None, "superseded", None, &now)?;
                diesel::insert_into(card_sessions::table)
                    .values(&row)
                    .execute(conn)?;
                Ok(row)
            })
        })
        .await
    }

    /// Close the open run of `session_id` on `card_id`. Idempotent: the
    /// first close wins (an MCP terminal tool closes before the completion
    /// listener's fallback gets there); returns whether a row was closed.
    /// `summary = None` leaves any recorded summary untouched.
    pub async fn close_card_run(
        &self,
        card_id: &str,
        session_id: &str,
        outcome: &str,
        summary: Option<String>,
    ) -> anyhow::Result<bool> {
        let card_id = card_id.to_string();
        let session_id = session_id.to_string();
        let outcome = outcome.to_string();
        let now = chrono::Utc::now().to_rfc3339();
        self.with_conn(move |conn| {
            let summary = summary.as_deref().filter(|s| !s.trim().is_empty());
            Ok(close_runs(conn, &card_id, Some(&session_id), &outcome, summary, &now)? > 0)
        })
        .await
    }

    /// Close whatever run is open on `card_id` (the user moved / stopped the
    /// card and the acting session isn't known). Returns rows closed.
    pub async fn close_open_card_runs(
        &self,
        card_id: &str,
        outcome: &str,
    ) -> anyhow::Result<usize> {
        let card_id = card_id.to_string();
        let outcome = outcome.to_string();
        let now = chrono::Utc::now().to_rfc3339();
        self.with_conn(move |conn| close_runs(conn, &card_id, None, &outcome, None, &now))
            .await
    }

    /// The newest run of `session_id` on `card_id`.
    pub async fn latest_card_run(
        &self,
        card_id: &str,
        session_id: &str,
    ) -> anyhow::Result<Option<CardSession>> {
        let card_id = card_id.to_string();
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            card_sessions::table
                .filter(card_sessions::card_id.eq(&card_id))
                .filter(card_sessions::session_id.eq(&session_id))
                .order(card_sessions::started_at.desc())
                .select(CardSession::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// A card's run history, newest first, joined with the session each run
    /// names (`session_exists = false` once that session was deleted).
    pub async fn list_card_sessions(&self, card_id: &str) -> anyhow::Result<Vec<CardSessionEntry>> {
        let card_id = card_id.to_string();
        self.with_conn(move |conn| {
            // (run, session id, session name, sealed_at) — session columns
            // are NULL once the session was deleted.
            type Row = (CardSession, Option<String>, Option<String>, Option<String>);
            let rows: Vec<Row> = card_sessions::table
                .left_join(sessions::table.on(sessions::id.eq(card_sessions::session_id)))
                .filter(card_sessions::card_id.eq(&card_id))
                .order((card_sessions::started_at.desc(), card_sessions::id.desc()))
                .select((
                    CardSession::as_select(),
                    sessions::id.nullable(),
                    sessions::name.nullable(),
                    sessions::sealed_at.nullable(),
                ))
                .load(conn)?;
            Ok(rows
                .into_iter()
                .map(|(r, sid, name, sealed_at)| CardSessionEntry {
                    id: r.id,
                    card_id: r.card_id,
                    session_id: r.session_id,
                    session_name: name,
                    session_exists: sid.is_some(),
                    sealed: sealed_at.is_some(),
                    step: r.step,
                    role: r.role,
                    model: r.model,
                    started_at: r.started_at,
                    ended_at: r.ended_at,
                    outcome: r.outcome,
                    summary: r.summary,
                })
                .collect())
        })
        .await
    }

    /// Cards in `project_id` created at/after `since` whose description
    /// names `origin_card_id` — the gap cards a reviewer files during its run
    /// (the review instructions require referencing the origin card id).
    pub async fn count_gap_cards_since(
        &self,
        project_id: &str,
        origin_card_id: &str,
        since: &str,
    ) -> anyhow::Result<i64> {
        let project_id = project_id.to_string();
        let pattern = format!("%{}%", origin_card_id.replace(['%', '_'], ""));
        let origin = origin_card_id.to_string();
        let since = since.to_string();
        self.with_conn(move |conn| {
            cards::table
                .filter(cards::project_id.eq(&project_id))
                .filter(cards::id.ne(&origin))
                .filter(cards::created_at.ge(&since))
                .filter(cards::description.like(&pattern))
                .count()
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Seal `session_id`: stamp `sealed_at` / `sealed_reason`, drop its
    /// pending queued messages, sever its resume link (`worker_step`), and
    /// close any run it still has open as `superseded`. Idempotent.
    ///
    /// `reason = None` derives it from how the session's newest run on its
    /// card ended (see [`seal_reason_for_outcome`]); a run still open means
    /// it was cut short by a newer one (`superseded`).
    ///
    /// The caller must already have terminated the session's agent (or, for
    /// a session sealing itself from inside its final MCP call, be ending
    /// that turn) and should hold its per-session lock, so no dispatcher can
    /// be between its sealed check and the spawn (see
    /// `SessionManager::send_message_locked`).
    pub async fn seal_session(
        &self,
        session_id: &str,
        reason: Option<&str>,
    ) -> anyhow::Result<SealOutcome> {
        let session_id = session_id.to_string();
        let reason = reason.map(str::to_string);
        let now = chrono::Utc::now().to_rfc3339();
        self.with_conn(move |conn| {
            conn.transaction(|conn| {
                let row: Option<(Option<String>, Option<String>)> = sessions::table
                    .find(&session_id)
                    .select((sessions::sealed_at, sessions::card_id))
                    .first(conn)
                    .optional()?;
                let Some((sealed_at, card_id)) = row else {
                    return Ok(SealOutcome::NotFound);
                };
                if sealed_at.is_some() {
                    return Ok(SealOutcome::AlreadySealed);
                }
                let reason = match reason {
                    Some(r) => r,
                    None => {
                        let last: Option<(Option<String>, Option<String>)> = card_sessions::table
                            .filter(card_sessions::session_id.eq(&session_id))
                            .order(card_sessions::started_at.desc())
                            .select((card_sessions::ended_at, card_sessions::outcome))
                            .first(conn)
                            .optional()?;
                        match last {
                            Some((Some(_), outcome)) => {
                                seal_reason_for_outcome(outcome.as_deref()).to_string()
                            }
                            _ => "superseded".to_string(),
                        }
                    }
                };
                diesel::update(sessions::table.find(&session_id))
                    .set((
                        sessions::sealed_at.eq(Some(&now)),
                        sessions::sealed_reason.eq(Some(&reason)),
                        sessions::worker_step.eq::<Option<String>>(None),
                    ))
                    .execute(conn)?;
                let dropped_queued = diesel::delete(
                    queued_messages::table.filter(queued_messages::session_id.eq(&session_id)),
                )
                .execute(conn)?;
                if let Some(card_id) = card_id {
                    close_runs(conn, &card_id, Some(&session_id), "superseded", None, &now)?;
                }
                Ok(SealOutcome::Sealed { dropped_queued })
            })
        })
        .await
    }
}
