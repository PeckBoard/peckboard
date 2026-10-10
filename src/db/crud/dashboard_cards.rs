//! Aggregate reads behind the card-centric dashboard widgets
//! (`/api/dashboard/attention`, `/review-queue`, `/review-quality`).
//! Each method is one or two set queries — never a query per card or
//! session. `scope` narrows to one project; `None` means every project.
//!
//! Projects and cards are shared across users (see `auth::access`), so only
//! chat plans (no project) are filtered per caller.

use diesel::prelude::*;
use diesel::sql_types::{BigInt, Bool, Nullable, Text};

use crate::db::Db;

/// Most rows any one attention source returns (the endpoint caps the
/// merged list at the same number).
pub const ATTENTION_LIMIT: i64 = 50;

/// An unresolved `question` event on a worker session.
#[derive(QueryableByName, Debug, Clone)]
pub struct PendingQuestionRow {
    #[diesel(sql_type = Text)]
    pub event_id: String,
    #[diesel(sql_type = Text)]
    pub session_id: String,
    /// Millis since the Unix epoch.
    #[diesel(sql_type = BigInt)]
    pub ts: i64,
    #[diesel(sql_type = Text)]
    pub data: String,
    #[diesel(sql_type = Nullable<Text>)]
    pub project_id: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub project_name: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub card_id: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub card_title: Option<String>,
}

/// A plan awaiting approval (`status = 'proposed'`).
#[derive(QueryableByName, Debug, Clone)]
pub struct ProposedPlanRow {
    #[diesel(sql_type = Text)]
    pub id: String,
    #[diesel(sql_type = Text)]
    pub title: String,
    #[diesel(sql_type = Text)]
    pub session_id: String,
    #[diesel(sql_type = Text)]
    pub updated_at: String,
    #[diesel(sql_type = Nullable<Text>)]
    pub project_id: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub project_name: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub card_id: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub card_title: Option<String>,
}

/// A card that is blocked, has an unmerged worktree, or whose latest run
/// crashed, with that latest run (`card_sessions`) alongside.
#[derive(QueryableByName, Debug, Clone)]
pub struct CardAttentionRow {
    #[diesel(sql_type = Text)]
    pub id: String,
    #[diesel(sql_type = Text)]
    pub title: String,
    #[diesel(sql_type = Text)]
    pub project_id: String,
    #[diesel(sql_type = Text)]
    pub project_name: String,
    #[diesel(sql_type = Text)]
    pub step: String,
    #[diesel(sql_type = Bool)]
    pub blocked: bool,
    #[diesel(sql_type = Nullable<Text>)]
    pub block_reason: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub worktree_unmerged_reason: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub worktree_unmerged_detail: Option<String>,
    #[diesel(sql_type = Text)]
    pub updated_at: String,
    #[diesel(sql_type = Nullable<Text>)]
    pub worker_session_id: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub last_worker_session_id: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub last_outcome: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub last_session_id: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub last_ended_at: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub last_summary: Option<String>,
}

/// A card sitting in the review step.
#[derive(QueryableByName, Debug, Clone)]
pub struct InReviewRow {
    #[diesel(sql_type = Text)]
    pub card_id: String,
    #[diesel(sql_type = Text)]
    pub title: String,
    #[diesel(sql_type = Text)]
    pub project_id: String,
    #[diesel(sql_type = Text)]
    pub project_name: String,
    #[diesel(sql_type = Nullable<Text>)]
    pub worker_session_id: Option<String>,
    #[diesel(sql_type = Text)]
    pub updated_at: String,
}

/// A card's latest review verdict.
#[derive(QueryableByName, Debug, Clone)]
pub struct ReviewedRow {
    #[diesel(sql_type = Text)]
    pub card_id: String,
    #[diesel(sql_type = Text)]
    pub title: String,
    #[diesel(sql_type = Text)]
    pub project_id: String,
    #[diesel(sql_type = Text)]
    pub project_name: String,
    #[diesel(sql_type = Text)]
    pub verdict: String,
    #[diesel(sql_type = Text)]
    pub reviewed_at: String,
    #[diesel(sql_type = Nullable<Text>)]
    pub reviewer_model: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub summary: Option<String>,
}

/// Review verdicts and crashes on one UTC day.
#[derive(QueryableByName, Debug, Clone, PartialEq)]
pub struct QualityDayRow {
    #[diesel(sql_type = Text)]
    pub date: String,
    #[diesel(sql_type = BigInt)]
    pub pass: i64,
    #[diesel(sql_type = BigInt)]
    pub changes_requested: i64,
    #[diesel(sql_type = BigInt)]
    pub crashes: i64,
}

/// Review verdicts for one project over the window.
#[derive(QueryableByName, Debug, Clone, PartialEq)]
pub struct QualityProjectRow {
    #[diesel(sql_type = Text)]
    pub project_id: String,
    #[diesel(sql_type = Text)]
    pub project_name: String,
    #[diesel(sql_type = BigInt)]
    pub pass: i64,
    #[diesel(sql_type = BigInt)]
    pub changes_requested: i64,
}

#[derive(QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

/// Review-quality aggregates since a UTC date (see
/// [`Db::dashboard_review_quality`]).
#[derive(Debug, Clone, Default)]
pub struct ReviewQuality {
    /// Only days with any activity; the route fills the gaps.
    pub days: Vec<QualityDayRow>,
    pub retries: i64,
    pub by_project: Vec<QualityProjectRow>,
}

/// `json_extract` raises on malformed JSON; plugin-written event data is
/// stored verbatim, so every extract is guarded.
const RESOLVED_ID: &str = "CASE WHEN json_valid(r.data) THEN COALESCE(\
    json_extract(r.data, '$.question_id'), json_extract(r.data, '$.questionId')) END";

impl Db {
    /// Unresolved worker questions (a `question` event with no matching
    /// `question-resolved`, either id spelling), newest first — the same
    /// bookkeeping as `/api/projects/{id}/pending-questions`.
    pub async fn dashboard_pending_questions(
        &self,
        scope: Option<String>,
    ) -> anyhow::Result<Vec<PendingQuestionRow>> {
        self.with_conn(move |conn| {
            let sql = format!(
                "SELECT e.id AS event_id, e.session_id, e.ts, e.data, \
                        s.project_id, pr.name AS project_name, \
                        COALESCE(s.card_id, CASE WHEN json_valid(e.data) \
                            THEN json_extract(e.data, '$.cardId') END) AS card_id, \
                        c.title AS card_title \
                   FROM sessions s \
                   JOIN events e ON e.session_id = s.id AND e.kind = 'question' \
                   LEFT JOIN projects pr ON pr.id = s.project_id \
                   LEFT JOIN cards c ON c.id = COALESCE(s.card_id, CASE WHEN json_valid(e.data) \
                            THEN json_extract(e.data, '$.cardId') END) \
                  WHERE s.is_worker = 1 AND s.project_id IS NOT NULL \
                    AND (? IS NULL OR s.project_id = ?) \
                    AND NOT EXISTS (SELECT 1 FROM events r \
                         WHERE r.session_id = e.session_id AND r.kind = 'question-resolved' \
                           AND {RESOLVED_ID} = e.id) \
                  ORDER BY e.ts DESC LIMIT ?"
            );
            diesel::sql_query(sql)
                .bind::<Nullable<Text>, _>(scope.clone())
                .bind::<Nullable<Text>, _>(scope)
                .bind::<BigInt, _>(ATTENTION_LIMIT)
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Plans awaiting approval, newest first. Board plans (with a project)
    /// are shared; a chat plan is visible only to its creator session's
    /// owner, or to admins — the `/api/plans` rule.
    pub async fn dashboard_proposed_plans(
        &self,
        scope: Option<String>,
        user_id: &str,
        is_admin: bool,
    ) -> anyhow::Result<Vec<ProposedPlanRow>> {
        let user_id = user_id.to_string();
        self.with_conn(move |conn| {
            diesel::sql_query(
                "SELECT p.id, p.title, p.session_id, p.updated_at, p.project_id, \
                        pr.name AS project_name, p.card_id, c.title AS card_title \
                   FROM plans p \
                   LEFT JOIN sessions s ON s.id = p.session_id \
                   LEFT JOIN projects pr ON pr.id = p.project_id \
                   LEFT JOIN cards c ON c.id = p.card_id \
                  WHERE p.status = 'proposed' \
                    AND (? IS NULL OR p.project_id = ?) \
                    AND (? OR p.project_id IS NOT NULL OR s.user_id = ?) \
                  ORDER BY p.updated_at DESC LIMIT ?",
            )
            .bind::<Nullable<Text>, _>(scope.clone())
            .bind::<Nullable<Text>, _>(scope)
            .bind::<Bool, _>(is_admin)
            .bind::<Text, _>(user_id)
            .bind::<BigInt, _>(ATTENTION_LIMIT)
            .load(conn)
            .map_err(Into::into)
        })
        .await
    }

    /// Cards needing a human: blocked or crashed-last-run (both only while
    /// the card is still open), or carrying an unmerged worktree (any step —
    /// the merge runs after `done`). Newest-updated first.
    pub async fn dashboard_card_attention(
        &self,
        scope: Option<String>,
    ) -> anyhow::Result<Vec<CardAttentionRow>> {
        self.with_conn(move |conn| {
            diesel::sql_query(
                "WITH last_run AS ( \
                    SELECT card_id, outcome, session_id, ended_at, summary, \
                           ROW_NUMBER() OVER (PARTITION BY card_id \
                                              ORDER BY started_at DESC, id DESC) AS rn \
                      FROM card_sessions) \
                 SELECT c.id, c.title, c.project_id, pr.name AS project_name, c.step, \
                        c.blocked, c.block_reason, c.worktree_unmerged_reason, \
                        c.worktree_unmerged_detail, c.updated_at, c.worker_session_id, \
                        c.last_worker_session_id, lr.outcome AS last_outcome, \
                        lr.session_id AS last_session_id, lr.ended_at AS last_ended_at, \
                        lr.summary AS last_summary \
                   FROM cards c \
                   JOIN projects pr ON pr.id = c.project_id \
                   LEFT JOIN last_run lr ON lr.card_id = c.id AND lr.rn = 1 \
                  WHERE (? IS NULL OR c.project_id = ?) \
                    AND (c.worktree_unmerged_reason IS NOT NULL \
                         OR (c.step NOT IN ('done', 'wont_do') \
                             AND (c.blocked = 1 \
                                  OR (lr.outcome = 'crashed' AND c.worker_session_id IS NULL)))) \
                  ORDER BY c.updated_at DESC LIMIT ?",
            )
            .bind::<Nullable<Text>, _>(scope.clone())
            .bind::<Nullable<Text>, _>(scope)
            .bind::<BigInt, _>(ATTENTION_LIMIT * 3)
            .load(conn)
            .map_err(Into::into)
        })
        .await
    }

    /// Cards in `review_step`, most recently updated first (max 50).
    pub async fn dashboard_in_review(
        &self,
        scope: Option<String>,
        review_step: &str,
    ) -> anyhow::Result<Vec<InReviewRow>> {
        let review_step = review_step.to_string();
        self.with_conn(move |conn| {
            diesel::sql_query(
                "SELECT c.id AS card_id, c.title, c.project_id, pr.name AS project_name, \
                        c.worker_session_id, c.updated_at \
                   FROM cards c JOIN projects pr ON pr.id = c.project_id \
                  WHERE c.step = ? AND (? IS NULL OR c.project_id = ?) \
                  ORDER BY c.updated_at DESC LIMIT 50",
            )
            .bind::<Text, _>(review_step)
            .bind::<Nullable<Text>, _>(scope.clone())
            .bind::<Nullable<Text>, _>(scope)
            .load(conn)
            .map_err(Into::into)
        })
        .await
    }

    /// The `limit` most recently reviewed cards with their verdict and the
    /// model of their latest review run.
    pub async fn dashboard_recent_reviews(
        &self,
        scope: Option<String>,
        limit: i64,
    ) -> anyhow::Result<Vec<ReviewedRow>> {
        self.with_conn(move |conn| {
            diesel::sql_query(
                "SELECT c.id AS card_id, c.title, c.project_id, pr.name AS project_name, \
                        c.review_verdict AS verdict, c.reviewed_at, \
                        (SELECT cs.model FROM card_sessions cs \
                          WHERE cs.card_id = c.id AND cs.role = 'review' \
                          ORDER BY cs.started_at DESC LIMIT 1) AS reviewer_model, \
                        c.review_summary AS summary \
                   FROM cards c JOIN projects pr ON pr.id = c.project_id \
                  WHERE c.reviewed_at IS NOT NULL AND c.review_verdict IS NOT NULL \
                    AND (? IS NULL OR c.project_id = ?) \
                  ORDER BY c.reviewed_at DESC LIMIT ?",
            )
            .bind::<Nullable<Text>, _>(scope.clone())
            .bind::<Nullable<Text>, _>(scope)
            .bind::<BigInt, _>(limit)
            .load(conn)
            .map_err(Into::into)
        })
        .await
    }

    /// Review outcomes and worker crashes from `card_sessions` since
    /// `since` (a `YYYY-MM-DD` UTC date; timestamps are stored as UTC
    /// RFC3339, so the date prefix is the UTC day):
    ///
    /// - pass / changes_requested: review runs (`role = 'review'`) that
    ///   ended `reviewed` / `changes_requested`, by end date.
    /// - crashes: runs that ended `crashed` (worker exited mid-turn,
    ///   dispatch failure, or the watchdog clearing a dead worker).
    /// - retries: runs started in the window whose previous run on the same
    ///   card had crashed.
    pub async fn dashboard_review_quality(
        &self,
        scope: Option<String>,
        since: &str,
    ) -> anyhow::Result<ReviewQuality> {
        let since = since.to_string();
        self.with_conn(move |conn| {
            let days: Vec<QualityDayRow> = diesel::sql_query(
                "SELECT substr(cs.ended_at, 1, 10) AS date, \
                        COALESCE(SUM(cs.role = 'review' AND cs.outcome = 'reviewed'), 0) AS pass, \
                        COALESCE(SUM(cs.role = 'review' AND cs.outcome = 'changes_requested'), 0) \
                            AS changes_requested, \
                        COALESCE(SUM(cs.outcome = 'crashed'), 0) AS crashes \
                   FROM card_sessions cs JOIN cards c ON c.id = cs.card_id \
                  WHERE cs.ended_at IS NOT NULL AND cs.ended_at >= ? \
                    AND (? IS NULL OR c.project_id = ?) \
                  GROUP BY date ORDER BY date",
            )
            .bind::<Text, _>(&since)
            .bind::<Nullable<Text>, _>(scope.clone())
            .bind::<Nullable<Text>, _>(scope.clone())
            .load(conn)?;
            let retries = diesel::sql_query(
                "WITH runs AS ( \
                    SELECT card_id, started_at, \
                           LAG(outcome) OVER (PARTITION BY card_id \
                                              ORDER BY started_at, id) AS prev \
                      FROM card_sessions) \
                 SELECT COUNT(*) AS n FROM runs r JOIN cards c ON c.id = r.card_id \
                  WHERE r.prev = 'crashed' AND r.started_at >= ? \
                    AND (? IS NULL OR c.project_id = ?)",
            )
            .bind::<Text, _>(&since)
            .bind::<Nullable<Text>, _>(scope.clone())
            .bind::<Nullable<Text>, _>(scope.clone())
            .get_result::<CountRow>(conn)?
            .n;
            let by_project: Vec<QualityProjectRow> = diesel::sql_query(
                "SELECT c.project_id, pr.name AS project_name, \
                        COALESCE(SUM(cs.outcome = 'reviewed'), 0) AS pass, \
                        COALESCE(SUM(cs.outcome = 'changes_requested'), 0) AS changes_requested \
                   FROM card_sessions cs \
                   JOIN cards c ON c.id = cs.card_id \
                   JOIN projects pr ON pr.id = c.project_id \
                  WHERE cs.role = 'review' AND cs.outcome IN ('reviewed', 'changes_requested') \
                    AND cs.ended_at >= ? AND (? IS NULL OR c.project_id = ?) \
                  GROUP BY c.project_id, pr.name \
                  ORDER BY pass + changes_requested DESC, pr.name",
            )
            .bind::<Text, _>(&since)
            .bind::<Nullable<Text>, _>(scope.clone())
            .bind::<Nullable<Text>, _>(scope)
            .load(conn)?;
            Ok(ReviewQuality {
                days,
                retries,
                by_project,
            })
        })
        .await
    }
}
