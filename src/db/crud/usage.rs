use diesel::prelude::*;
use diesel::sql_types::{BigInt, Bool, Nullable, Text};

use crate::db::Db;
use crate::db::models::{NewUsageEvent, UsageEvent};
use crate::db::schema::usage_events;

/// One grouped-aggregate row from a usage rollup query: the summed token
/// slices for a single (entity, model) pair. Rollups group by model as well
/// as entity so the route layer can price each model's tokens at its own
/// rate before folding the per-model rows into one [`crate::routes::usage::EntityUsage`]
/// — summing tokens across models first and pricing once would misprice any
/// entity that used more than one model.
///
/// `context_tokens` is the peak (MAX) context-window occupancy across the
/// group, not a sum: context is a per-turn snapshot that overlaps across
/// turns, so summing it would be meaningless.
#[derive(QueryableByName, Debug, Clone)]
pub struct UsageRollupRow {
    #[diesel(sql_type = Text)]
    pub entity_id: String,
    #[diesel(sql_type = Text)]
    pub entity_name: String,
    #[diesel(sql_type = Nullable<Text>)]
    pub model: Option<String>,
    /// Owning project: the card's `project_id` for the per-card rollup, the
    /// session's `project_id` for the per-session rollups; `NULL` for the
    /// project rollup itself, where it has no meaning. Lets the frontend
    /// filter cards/sessions by project without a second round-trip.
    #[diesel(sql_type = Nullable<Text>)]
    pub project_id: Option<String>,
    /// Session flags, meaningful only for session-grained rollups (session /
    /// expert / single-session); always `false` for project/card rollups.
    /// Lets the frontend split chats vs workers vs experts without joining
    /// against the sessions list client-side.
    #[diesel(sql_type = Bool)]
    pub is_worker: bool,
    #[diesel(sql_type = Bool)]
    pub is_expert: bool,
    #[diesel(sql_type = BigInt)]
    pub input_tokens: i64,
    #[diesel(sql_type = BigInt)]
    pub output_tokens: i64,
    #[diesel(sql_type = BigInt)]
    pub cache_read_tokens: i64,
    #[diesel(sql_type = BigInt)]
    pub cache_creation_tokens: i64,
    #[diesel(sql_type = BigInt)]
    pub total_tokens: i64,
    #[diesel(sql_type = BigInt)]
    pub context_tokens: i64,
}

/// Shared SELECT-list fragment for every rollup query. `COALESCE` guards keep
/// the columns non-null so they map onto `UsageRollupRow`'s `BigInt` fields
/// even for empty groups; `MAX` on `context_tokens` reflects peak occupancy.
const ROLLUP_AGG_COLS: &str = "\
    COALESCE(SUM(u.input_tokens), 0) AS input_tokens, \
    COALESCE(SUM(u.output_tokens), 0) AS output_tokens, \
    COALESCE(SUM(u.cache_read_tokens), 0) AS cache_read_tokens, \
    COALESCE(SUM(u.cache_creation_tokens), 0) AS cache_creation_tokens, \
    COALESCE(SUM(u.total_tokens), 0) AS total_tokens, \
    COALESCE(MAX(u.context_tokens), 0) AS context_tokens";

impl Db {
    /// Insert a per-turn usage row. If `new.turn_seq` is `None`, the next
    /// per-session turn number is assigned (max + 1), mirroring how
    /// `append_event` assigns event seqs.
    pub async fn record_usage_event(&self, mut new: NewUsageEvent) -> anyhow::Result<UsageEvent> {
        self.with_conn(move |conn| {
            if new.turn_seq.is_none() {
                let next: i32 = usage_events::table
                    .filter(usage_events::session_id.eq(&new.session_id))
                    .select(diesel::dsl::max(usage_events::turn_seq))
                    .first::<Option<i32>>(conn)?
                    .map(|s| s + 1)
                    .unwrap_or(1);
                new.turn_seq = Some(next);
            }
            diesel::insert_into(usage_events::table)
                .values(&new)
                .returning(UsageEvent::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// All usage rows for a session, oldest-first by timestamp.
    pub async fn usage_events_for_session(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Vec<UsageEvent>> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            usage_events::table
                .filter(usage_events::session_id.eq(&session_id))
                .select(UsageEvent::as_select())
                .order((usage_events::ts.asc(), usage_events::turn_seq.asc()))
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Usage rows for a session whose `ts` falls within `[start_ts, end_ts]`
    /// (inclusive on both ends), oldest-first. Uses the
    /// `(session_id, ts)` index.
    pub async fn usage_events_for_session_in_range(
        &self,
        session_id: &str,
        start_ts: i64,
        end_ts: i64,
    ) -> anyhow::Result<Vec<UsageEvent>> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            usage_events::table
                .filter(usage_events::session_id.eq(&session_id))
                .filter(usage_events::ts.ge(start_ts))
                .filter(usage_events::ts.le(end_ts))
                .select(UsageEvent::as_select())
                .order((usage_events::ts.asc(), usage_events::turn_seq.asc()))
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Per-session usage, one [`UsageRollupRow`] per (session, model). Every
    /// session that has any usage row appears; the join supplies the session
    /// name. Aggregation happens in SQL — rows are never loaded individually.
    pub async fn usage_rollup_by_session(&self) -> anyhow::Result<Vec<UsageRollupRow>> {
        let sql = format!(
            "SELECT u.session_id AS entity_id, s.name AS entity_name, u.model AS model, \
             s.project_id AS project_id, s.is_worker AS is_worker, s.is_expert AS is_expert, \
             {ROLLUP_AGG_COLS} \
             FROM usage_events u \
             JOIN sessions s ON s.id = u.session_id \
             GROUP BY u.session_id, u.model"
        );
        self.with_conn(move |conn| {
            diesel::sql_query(sql)
                .load::<UsageRollupRow>(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Per-project usage (sessions joined through `sessions.project_id`), one
    /// row per (project, model). Sessions with a null `project_id` are
    /// excluded by the join.
    pub async fn usage_rollup_by_project(&self) -> anyhow::Result<Vec<UsageRollupRow>> {
        let sql = format!(
            "SELECT s.project_id AS entity_id, p.name AS entity_name, u.model AS model, \
             CAST(NULL AS TEXT) AS project_id, 0 AS is_worker, 0 AS is_expert, \
             {ROLLUP_AGG_COLS} \
             FROM usage_events u \
             JOIN sessions s ON s.id = u.session_id \
             JOIN projects p ON p.id = s.project_id \
             GROUP BY s.project_id, u.model"
        );
        self.with_conn(move |conn| {
            diesel::sql_query(sql)
                .load::<UsageRollupRow>(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Per-card usage (sessions joined through `sessions.card_id`), one row
    /// per (card, model). Sessions with a null `card_id` are excluded.
    pub async fn usage_rollup_by_card(&self) -> anyhow::Result<Vec<UsageRollupRow>> {
        let sql = format!(
            "SELECT s.card_id AS entity_id, c.title AS entity_name, u.model AS model, \
             c.project_id AS project_id, 0 AS is_worker, 0 AS is_expert, \
             {ROLLUP_AGG_COLS} \
             FROM usage_events u \
             JOIN sessions s ON s.id = u.session_id \
             JOIN cards c ON c.id = s.card_id \
             GROUP BY s.card_id, u.model"
        );
        self.with_conn(move |conn| {
            diesel::sql_query(sql)
                .load::<UsageRollupRow>(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Per-expert usage. Each expert *session* (`is_expert = 1`) is its own
    /// entity (id = session id, name = session name) — experts are distinct
    /// long-lived sessions, not pooled by `expert_kind`. One row per
    /// (expert session, model).
    pub async fn usage_rollup_by_expert(&self) -> anyhow::Result<Vec<UsageRollupRow>> {
        let sql = format!(
            "SELECT u.session_id AS entity_id, s.name AS entity_name, u.model AS model, \
             s.project_id AS project_id, s.is_worker AS is_worker, s.is_expert AS is_expert, \
             {ROLLUP_AGG_COLS} \
             FROM usage_events u \
             JOIN sessions s ON s.id = u.session_id \
             WHERE s.is_expert = 1 \
             GROUP BY u.session_id, u.model"
        );
        self.with_conn(move |conn| {
            diesel::sql_query(sql)
                .load::<UsageRollupRow>(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Usage for a single session, one row per model used. Empty when the
    /// session has no usage rows (the caller supplies the name/zeros in that
    /// case). Uses the `(session_id, ts)` index via the equality filter.
    pub async fn usage_rollup_for_session(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Vec<UsageRollupRow>> {
        let session_id = session_id.to_string();
        let sql = format!(
            "SELECT u.session_id AS entity_id, s.name AS entity_name, u.model AS model, \
             s.project_id AS project_id, s.is_worker AS is_worker, s.is_expert AS is_expert, \
             {ROLLUP_AGG_COLS} \
             FROM usage_events u \
             JOIN sessions s ON s.id = u.session_id \
             WHERE u.session_id = ? \
             GROUP BY u.session_id, u.model"
        );
        self.with_conn(move |conn| {
            diesel::sql_query(sql)
                .bind::<Text, _>(session_id)
                .load::<UsageRollupRow>(conn)
                .map_err(Into::into)
        })
        .await
    }
}
