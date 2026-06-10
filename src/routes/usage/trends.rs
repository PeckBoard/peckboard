//! Time-bucketed usage trend series — `GET /api/usage/trends`.
//!
//! Returns the `TrendSeries`/`TrendPoint` shapes from the contract
//! ([`super`]) as a flat JSON array, so the dashboard can chart how tokens
//! and cost move over time. Two families of trend are served by the one
//! endpoint:
//!
//! - **Entity trends** (`entity=session|project|card|expert`, or omitted for
//!   an install-wide `overall` series): bucketed sums over `usage_events`,
//!   joined to `sessions` for project/card/expert attribution. Tokens come
//!   from the rows; `est_cost` is priced per-model in Rust through the shared
//!   [`super::cost::usage_cost`] (there is no cost column to `SUM`), so the
//!   numbers match every other usage panel.
//! - **Operation-kind trends** (`entity=operation`): the per-operation costs
//!   the sibling endpoint derives, bucketed over time into one series per
//!   `file_update | ask_expert | qa`. We reuse
//!   [`super::operations::all_operation_costs`] rather than re-deriving the
//!   attribution, so a bucket here is exactly the sum of the operations that
//!   land in it.
//!
//! Bounded by construction: the time window is clamped so a single series can
//! never exceed [`MAX_BUCKETS`] buckets, and the per-entity SQL groups on the
//! indexed `ts` column.

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::get,
};
use diesel::prelude::*;
use diesel::sql_query;
use diesel::sql_types::{BigInt, Nullable, Text};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::Arc;

use super::cost::usage_cost;
use super::operations::{OperationScope, all_operation_costs};
use super::{OperationKind, TrendPoint, TrendSeries};
use crate::auth::middleware::require_auth;
use crate::state::AppState;

/// Milliseconds in one hour / one day — the two supported bucket widths.
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;

/// Hard ceiling on the number of buckets a single series may span. The time
/// window is clamped to at most this many buckets, so an unbounded
/// `?from=0&bucket=hour` request can't make the endpoint scan or emit a
/// runaway series. At `hour` granularity this is ~62 days of history; at
/// `day`, ~4 years — past that the caller pages with explicit `from`/`to`.
const MAX_BUCKETS: i64 = 1500;

#[derive(Deserialize)]
struct TrendsQuery {
    /// Which figure the series is "about" — `tokens` (default) or `cost`.
    /// Every [`TrendPoint`] carries both `tokens` and `est_cost` regardless;
    /// this only labels the series, since computing both is free.
    metric: Option<String>,
    /// `session | project | card | expert | operation`. Omitted ⇒ a single
    /// install-wide `overall` series.
    entity: Option<String>,
    /// Optional narrowing within `entity`: a specific entity id for
    /// session/project/card/expert, or an operation kind
    /// (`file_update|ask_expert|qa`) for `operation`. Omitted ⇒ one series
    /// per entity of that kind.
    id: Option<String>,
    /// `hour` (default) or `day`.
    bucket: Option<String>,
    /// Inclusive window start, epoch ms. Defaults so the window is the most
    /// recent [`MAX_BUCKETS`] buckets ending at `to`.
    from: Option<i64>,
    /// Exclusive window end, epoch ms. Defaults to now.
    to: Option<i64>,
}

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/usage/trends", get(get_trends))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

type Err400Or500 = (StatusCode, Json<serde_json::Value>);

fn bad_request(msg: &str) -> Err400Or500 {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": msg })),
    )
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// GET /api/usage/trends — see the module docs. Returns a JSON array of
/// [`TrendSeries`]; an empty array when nothing falls in the window (never a
/// 404).
async fn get_trends(
    State(state): State<Arc<AppState>>,
    Query(params): Query<TrendsQuery>,
) -> impl IntoResponse {
    let bucket_ms = match params.bucket.as_deref().unwrap_or("hour") {
        "hour" => HOUR_MS,
        "day" => DAY_MS,
        _ => return Err(bad_request("bucket must be 'hour' or 'day'")),
    };

    let metric = match params.metric.as_deref().unwrap_or("tokens") {
        m @ ("tokens" | "cost") => m.to_string(),
        _ => return Err(bad_request("metric must be 'tokens' or 'cost'")),
    };

    // Resolve and clamp the window so a single series never exceeds
    // MAX_BUCKETS buckets. `to` is exclusive, `from` inclusive.
    let to = params.to.unwrap_or_else(now_ms);
    let max_span = MAX_BUCKETS * bucket_ms;
    let mut from = params.from.unwrap_or(to - max_span);
    if to < from {
        return Err(bad_request("'to' must be >= 'from'"));
    }
    if to - from > max_span {
        from = to - max_span;
    }

    let series = match params.entity.as_deref() {
        None | Some("overall") | Some("session") | Some("project") | Some("card")
        | Some("expert") => {
            entity_trends(
                &state,
                params.entity.as_deref(),
                params.id.as_deref(),
                from,
                to,
                bucket_ms,
                &metric,
            )
            .await
        }
        Some("operation") => {
            operation_trends(&state, params.id.as_deref(), from, to, bucket_ms, &metric).await
        }
        Some(_) => {
            return Err(bad_request(
                "entity must be one of session|project|card|expert|operation",
            ));
        }
    };

    let series = series.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    Ok::<_, Err400Or500>(Json(series))
}

/// One grouped aggregation row: token sums for a single
/// `(entity_id, bucket_ts, model)`. Cost is computed in Rust from the slices
/// because pricing is per-model and there is no cost column to `SUM`.
#[derive(QueryableByName)]
struct TrendAggRow {
    #[diesel(sql_type = Text)]
    entity_id: String,
    #[diesel(sql_type = BigInt)]
    bucket_ts: i64,
    #[diesel(sql_type = Nullable<Text>)]
    model: Option<String>,
    #[diesel(sql_type = BigInt)]
    input_tokens: i64,
    #[diesel(sql_type = BigInt)]
    output_tokens: i64,
    #[diesel(sql_type = BigInt)]
    cache_read_tokens: i64,
    #[diesel(sql_type = BigInt)]
    cache_creation_tokens: i64,
    #[diesel(sql_type = BigInt)]
    total_tokens: i64,
}

/// Bucketed token/cost series over `usage_events` for the given entity kind.
///
/// `entity` is one of `session|project|card|expert` (or `None`/`overall` for
/// an install-wide series). With `id` set, a single series for that entity;
/// without, one series per entity of that kind.
async fn entity_trends(
    state: &Arc<AppState>,
    entity: Option<&str>,
    id: Option<&str>,
    from: i64,
    to: i64,
    bucket_ms: i64,
    metric: &str,
) -> anyhow::Result<Vec<TrendSeries>> {
    // (entity_id select expr, optional JOIN, extra WHERE, id filter column)
    // bucket_ms is a trusted in-process constant (HOUR_MS/DAY_MS), so it is
    // safe to inline; all caller-supplied values are bound parameters.
    let (entity_expr, join, extra_where, id_col): (&str, &str, &str, Option<&str>) = match entity {
        None | Some("overall") => ("'overall'", "", "", None),
        Some("session") => ("u.session_id", "", "", Some("u.session_id")),
        Some("project") => (
            "s.project_id",
            "JOIN sessions s ON s.id = u.session_id",
            "AND s.project_id IS NOT NULL",
            Some("s.project_id"),
        ),
        Some("card") => (
            "s.card_id",
            "JOIN sessions s ON s.id = u.session_id",
            "AND s.card_id IS NOT NULL",
            Some("s.card_id"),
        ),
        Some("expert") => (
            "u.session_id",
            "JOIN sessions s ON s.id = u.session_id",
            "AND s.is_expert = 1",
            Some("u.session_id"),
        ),
        _ => ("'overall'", "", "", None),
    };

    let id_filter = match (id, id_col) {
        (Some(_), Some(col)) => format!("AND {col} = ?3"),
        _ => String::new(),
    };

    let sql = format!(
        "SELECT {entity_expr} AS entity_id, \
                (u.ts / {bucket_ms}) * {bucket_ms} AS bucket_ts, \
                u.model AS model, \
                COALESCE(SUM(u.input_tokens), 0) AS input_tokens, \
                COALESCE(SUM(u.output_tokens), 0) AS output_tokens, \
                COALESCE(SUM(u.cache_read_tokens), 0) AS cache_read_tokens, \
                COALESCE(SUM(u.cache_creation_tokens), 0) AS cache_creation_tokens, \
                COALESCE(SUM(u.total_tokens), 0) AS total_tokens \
         FROM usage_events u {join} \
         WHERE u.ts >= ?1 AND u.ts < ?2 {extra_where} {id_filter} \
         GROUP BY entity_id, bucket_ts, u.model \
         ORDER BY bucket_ts ASC"
    );

    // `sql_query` binds change the query's type with each `.bind()`, so a
    // conditional third bind can't be reassigned to the same variable. Branch
    // the terminal `.load()` instead: the id filter adds exactly one `?3`.
    let id_owned = id.map(str::to_string);
    let rows: Vec<TrendAggRow> = state
        .db
        .with_conn(move |conn| {
            let q = sql_query(sql).bind::<BigInt, _>(from).bind::<BigInt, _>(to);
            let rows = match id_owned {
                Some(id) => q.bind::<Text, _>(id).load::<TrendAggRow>(conn)?,
                None => q.load::<TrendAggRow>(conn)?,
            };
            Ok(rows)
        })
        .await?;

    // Fold (entity_id -> bucket_ts -> accumulated tokens + cost). BTreeMaps
    // keep entities and buckets in ascending order, so each series' points
    // come out monotonically ordered by bucket_ts for free.
    let mut by_entity: BTreeMap<String, BTreeMap<i64, (i64, f64)>> = BTreeMap::new();
    for r in rows {
        let cost = usage_cost(
            r.model.as_deref(),
            r.input_tokens,
            r.output_tokens,
            r.cache_read_tokens,
            r.cache_creation_tokens,
        );
        let entry = by_entity
            .entry(r.entity_id)
            .or_default()
            .entry(r.bucket_ts)
            .or_insert((0, 0.0));
        entry.0 += r.total_tokens;
        entry.1 += cost;
    }

    Ok(by_entity
        .into_iter()
        .map(|(entity_id, buckets)| TrendSeries {
            metric: metric.to_string(),
            entity_id,
            points: buckets
                .into_iter()
                .map(|(bucket_ts, (tokens, est_cost))| TrendPoint {
                    bucket_ts,
                    tokens,
                    est_cost,
                })
                .collect(),
        })
        .collect())
}

/// Bucketed cost-over-time series per operation kind. Reuses the sibling
/// endpoint's per-operation derivation ([`all_operation_costs`]) so the
/// buckets are exactly the sum of the operations that fall in them; we only
/// add the time bucketing. With `id` set to a kind name, only that kind's
/// series is returned.
async fn operation_trends(
    state: &Arc<AppState>,
    id: Option<&str>,
    from: i64,
    to: i64,
    bucket_ms: i64,
    metric: &str,
) -> anyhow::Result<Vec<TrendSeries>> {
    let only_kind = match id {
        None => None,
        Some("file_update") => Some(OperationKind::FileUpdate),
        Some("ask_expert") => Some(OperationKind::AskExpert),
        Some("qa") => Some(OperationKind::Qa),
        Some(other) => {
            return Err(anyhow::anyhow!(
                "operation id must be file_update|ask_expert|qa, got '{other}'"
            ));
        }
    };

    let costs = all_operation_costs(&state.db, &OperationScope::Global).await?;

    let mut by_kind: BTreeMap<&'static str, BTreeMap<i64, (i64, f64)>> = BTreeMap::new();
    for c in costs {
        if c.ts < from || c.ts >= to {
            continue;
        }
        if let Some(k) = only_kind
            && c.kind != k
        {
            continue;
        }
        let bucket_ts = (c.ts / bucket_ms) * bucket_ms;
        let entry = by_kind
            .entry(operation_kind_str(c.kind))
            .or_default()
            .entry(bucket_ts)
            .or_insert((0, 0.0));
        entry.0 += c.tokens;
        entry.1 += c.est_cost;
    }

    Ok(by_kind
        .into_iter()
        .map(|(kind, buckets)| TrendSeries {
            metric: metric.to_string(),
            entity_id: kind.to_string(),
            points: buckets
                .into_iter()
                .map(|(bucket_ts, (tokens, est_cost))| TrendPoint {
                    bucket_ts,
                    tokens,
                    est_cost,
                })
                .collect(),
        })
        .collect())
}

/// Wire name for an [`OperationKind`] — matches its `serde(rename_all =
/// "snake_case")` serialization, used as the `entity_id` of an operation
/// series.
fn operation_kind_str(kind: OperationKind) -> &'static str {
    match kind {
        OperationKind::FileUpdate => "file_update",
        OperationKind::AskExpert => "ask_expert",
        OperationKind::Qa => "qa",
    }
}
