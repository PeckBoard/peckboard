//! Usage dashboard API contract — the shared response shapes and cost model
//! the whole usage feature is built against.
//!
//! This module is the stable seam that lets backend aggregation (a later
//! card) and the frontend panels be built in parallel. The contract is:
//! the structs here, their TypeScript mirrors in `web/src/types/api.ts`, and
//! the cost model in [`cost`]. Field names are bare snake_case (matching the
//! rest of `src/routes/`), so the TS interfaces read these directly with no
//! rename attributes on either side.
//!
//! Aggregation handlers are intentionally NOT here yet — the only live route
//! is `GET /api/usage/costs`, which serves the static rate table the
//! frontend caches. Later cards add the rollup/trend endpoints to this same
//! router.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::get,
};
use serde::Serialize;

use crate::auth::middleware::require_auth;
use crate::db::crud::UsageRollupRow;
use crate::state::AppState;

pub mod cost;
pub mod operations;
pub mod trends;
pub mod turns;

pub use cost::{CostTable, ModelRates, TokenKind};

/// What a [`EntityUsage`] row aggregates over. Serialized lowercase
/// (`session` | `project` | `card` | `expert`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Session,
    Project,
    Card,
    Expert,
}

/// Token totals + estimated cost for one entity (a session, project, card,
/// or expert). The four billed token slices plus the `total`/`context`
/// roll-ups, and the `est_cost` the backend computed from them via
/// [`cost::usage_cost`]. `est_cost` is in USD.
#[derive(Debug, Clone, Serialize)]
pub struct EntityUsage {
    pub id: String,
    pub name: String,
    pub kind: EntityKind,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    /// Provider-reported turn total. Overlaps the four billed slices, so it
    /// is a display roll-up only — never re-priced (see [`cost::usage_cost`]).
    pub total_tokens: i64,
    /// Latest context-window occupancy snapshot for the entity.
    pub context_tokens: i64,
    pub est_cost: f64,
    /// Owning project id — set only for `kind = card` rollups (from the
    /// card's `project_id`); `None` for session/project/expert kinds, where
    /// it has no meaning. Lets the frontend cards panel filter by a selected
    /// project without a second round-trip. Always serialized (as `null` when
    /// absent); the TS mirror marks it optional so existing consumers that
    /// never read it are unaffected.
    pub project_id: Option<String>,
}

/// A session row, which surfaces its lifetime token + context totals
/// explicitly on top of the shared [`EntityUsage`] fields (flattened, so the
/// wire shape is `EntityUsage` plus the two extra fields). The TS mirror is
/// `interface SessionUsage extends EntityUsage`.
#[derive(Debug, Clone, Serialize)]
pub struct SessionUsage {
    #[serde(flatten)]
    pub usage: EntityUsage,
    /// Sum of all billed tokens across the session's lifetime.
    pub total_tokens_used: i64,
    /// Most recent context-window occupancy for the session.
    pub total_context_tokens: i64,
    /// Session role flags, so the dashboard can split chats / workers /
    /// experts and route each to the right detail page.
    pub is_worker: bool,
    pub is_expert: bool,
}

/// What kind of operation an [`OperationCost`] attributes spend to.
/// Serialized snake_case (`file_update` | `file_read` | `ask_expert` | `qa`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    FileUpdate,
    /// Cache-read spend attributed to the files `Read` during each turn —
    /// only the turn's cache-read slice, since that is the cost of
    /// re-loading previously-seen file content into context.
    FileRead,
    AskExpert,
    Qa,
}

/// Cost attributed to a single operation — one file update, one
/// `ask_expert` round-trip, or one question/answer combination. `ref_id`
/// points at the underlying thing (file path, expert id, decision id) and
/// `label` is its human-readable name. `ts` is epoch milliseconds.
#[derive(Debug, Clone, Serialize)]
pub struct OperationCost {
    pub kind: OperationKind,
    pub ref_id: String,
    pub label: String,
    pub tokens: i64,
    pub est_cost: f64,
    pub ts: i64,
}

/// One point in a time-series. `bucket_ts` is the epoch-millisecond start of
/// the bucket; `est_cost` is USD.
#[derive(Debug, Clone, Serialize)]
pub struct TrendPoint {
    pub bucket_ts: i64,
    pub tokens: i64,
    pub est_cost: f64,
}

/// A named time-series for one entity — e.g. `metric: "tokens"` or
/// `"cost"` over time for a given session/project/card/expert.
#[derive(Debug, Clone, Serialize)]
pub struct TrendSeries {
    pub metric: String,
    pub entity_id: String,
    pub points: Vec<TrendPoint>,
}

/// Install-wide token + cost totals, summed across every entity.
#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageTotals {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub total_tokens: i64,
    pub context_tokens: i64,
    pub est_cost: f64,
}

/// The single-fetch envelope for the whole usage dashboard view: top-line
/// totals, the per-entity breakdowns, the per-operation cost list, and the
/// trend series the charts render. A later card fills the aggregation
/// handler that builds this; the shape is frozen here so the frontend can be
/// built against it in parallel.
#[derive(Debug, Clone, Serialize)]
pub struct UsageDashboard {
    pub totals: UsageTotals,
    pub sessions: Vec<SessionUsage>,
    pub projects: Vec<EntityUsage>,
    pub cards: Vec<EntityUsage>,
    pub experts: Vec<EntityUsage>,
    pub operations: Vec<OperationCost>,
    pub trends: Vec<TrendSeries>,
}

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/usage/costs", get(get_costs))
        .route("/api/usage/sessions", get(list_session_usage))
        .route("/api/usage/sessions/{id}", get(get_session_usage))
        .route(
            "/api/usage/sessions/{id}/turns",
            get(turns::get_session_turns),
        )
        .route("/api/usage/projects", get(list_project_usage))
        .route("/api/usage/cards", get(list_card_usage))
        .route("/api/usage/experts", get(list_expert_usage))
        .route("/api/usage/operations", get(operations::get_operations))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// GET /api/usage/costs — the per-model rate table the running binary prices
/// usage with. The frontend fetches this once and caches it, so its panels
/// show the same `est_cost` the backend computed without hardcoding rates of
/// their own. Rates live only in [`cost`].
async fn get_costs(State(_state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(cost::cost_table())
}

/// Mutable accumulator that folds the per-(entity, model) [`UsageRollupRow`]s
/// for one entity into a single [`EntityUsage`]. Token columns sum; cost is
/// accumulated per row at that row's model rate (so a multi-model entity is
/// priced correctly); `context_tokens` keeps the peak.
#[derive(Default)]
struct UsageAccumulator {
    name: String,
    project_id: Option<String>,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_creation_tokens: i64,
    total_tokens: i64,
    context_tokens: i64,
    est_cost: f64,
}

impl UsageAccumulator {
    fn add(&mut self, row: &UsageRollupRow) {
        if self.project_id.is_none() {
            self.project_id = row.project_id.clone();
        }
        self.input_tokens += row.input_tokens;
        self.output_tokens += row.output_tokens;
        self.cache_read_tokens += row.cache_read_tokens;
        self.cache_creation_tokens += row.cache_creation_tokens;
        self.total_tokens += row.total_tokens;
        self.context_tokens = self.context_tokens.max(row.context_tokens);
        self.est_cost += cost::usage_cost(
            row.model.as_deref(),
            row.input_tokens,
            row.output_tokens,
            row.cache_read_tokens,
            row.cache_creation_tokens,
        );
    }

    fn into_entity(self, id: String, kind: EntityKind) -> EntityUsage {
        EntityUsage {
            id,
            name: self.name,
            kind,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_creation_tokens: self.cache_creation_tokens,
            total_tokens: self.total_tokens,
            context_tokens: self.context_tokens,
            est_cost: self.est_cost,
            project_id: self.project_id,
        }
    }
}

/// Fold per-(entity, model) rows into one [`EntityUsage`] per entity, sorted
/// by `est_cost` descending (biggest spenders first) with a name tie-break so
/// the ordering is stable across requests.
fn fold_entities(rows: Vec<UsageRollupRow>, kind: EntityKind) -> Vec<EntityUsage> {
    let mut order: Vec<String> = Vec::new();
    let mut accs: HashMap<String, UsageAccumulator> = HashMap::new();
    for row in &rows {
        let acc = accs.entry(row.entity_id.clone()).or_insert_with(|| {
            order.push(row.entity_id.clone());
            UsageAccumulator::default()
        });
        if acc.name.is_empty() {
            acc.name = row.entity_name.clone();
        }
        acc.add(row);
    }
    let mut out: Vec<EntityUsage> = order
        .into_iter()
        .map(|id| {
            let acc = accs.remove(&id).expect("id was inserted into order");
            acc.into_entity(id, kind)
        })
        .collect();
    out.sort_by(|a, b| {
        b.est_cost
            .partial_cmp(&a.est_cost)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

/// Build a [`SessionUsage`] from a session's per-model rows. `total_tokens_used`
/// is the sum of the four billed slices (distinct from the provider-reported
/// `total_tokens` roll-up); `total_context_tokens` mirrors the peak context
/// snapshot.
fn session_usage(
    id: String,
    name: String,
    flags: (bool, bool),
    rows: Vec<UsageRollupRow>,
) -> SessionUsage {
    let mut acc = UsageAccumulator {
        name,
        ..Default::default()
    };
    for row in &rows {
        acc.add(row);
    }
    let total_tokens_used =
        acc.input_tokens + acc.output_tokens + acc.cache_read_tokens + acc.cache_creation_tokens;
    let context = acc.context_tokens;
    SessionUsage {
        usage: acc.into_entity(id, EntityKind::Session),
        total_tokens_used,
        total_context_tokens: context,
        is_worker: flags.0,
        is_expert: flags.1,
    }
}

fn db_error(e: anyhow::Error) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": e.to_string() })),
    )
}

/// GET /api/usage/sessions — per-session token totals + cost, one row per
/// session that has recorded usage.
async fn list_session_usage(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let rows = state.db.usage_rollup_by_session().await.map_err(db_error)?;
    let sessions: Vec<SessionUsage> = fold_session_rows(rows);
    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(sessions))
}

/// Group the flat per-(session, model) rows into one [`SessionUsage`] per
/// session, cost-sorted like the entity rollups.
fn fold_session_rows(rows: Vec<UsageRollupRow>) -> Vec<SessionUsage> {
    /// Per-session grouping slot: name, (is_worker, is_expert), rows.
    type Slot = (String, (bool, bool), Vec<UsageRollupRow>);
    let mut order: Vec<String> = Vec::new();
    let mut grouped: HashMap<String, Slot> = HashMap::new();
    for row in rows {
        let entry = grouped.entry(row.entity_id.clone()).or_insert_with(|| {
            order.push(row.entity_id.clone());
            (
                row.entity_name.clone(),
                (row.is_worker, row.is_expert),
                Vec::new(),
            )
        });
        entry.2.push(row);
    }
    let mut out: Vec<SessionUsage> = order
        .into_iter()
        .map(|id| {
            let (name, flags, rows) = grouped.remove(&id).expect("id was inserted into order");
            session_usage(id, name, flags, rows)
        })
        .collect();
    out.sort_by(|a, b| {
        b.usage
            .est_cost
            .partial_cmp(&a.usage.est_cost)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.usage.name.cmp(&b.usage.name))
    });
    out
}

/// GET /api/usage/sessions/:id — single-session breakdown. Returns zeros (not
/// 404) for a session that exists but has no usage yet; 404 only when the
/// session id is unknown.
async fn get_session_usage(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let rows = state
        .db
        .usage_rollup_for_session(&id)
        .await
        .map_err(db_error)?;
    if let Some((name, flags)) = rows
        .first()
        .map(|r| (r.entity_name.clone(), (r.is_worker, r.is_expert)))
    {
        return Ok(Json(session_usage(id, name, flags, rows)));
    }
    // No usage rows: fall back to the session record for its name, or 404.
    match state.db.get_session(&id).await.map_err(db_error)? {
        Some(session) => Ok(Json(session_usage(
            id,
            session.name,
            (session.is_worker, session.is_expert),
            Vec::new(),
        ))),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        )),
    }
}

/// GET /api/usage/projects — per-project token totals + cost.
async fn list_project_usage(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let rows = state.db.usage_rollup_by_project().await.map_err(db_error)?;
    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(fold_entities(rows, EntityKind::Project)))
}

/// GET /api/usage/cards — per-card token totals + cost.
async fn list_card_usage(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let rows = state.db.usage_rollup_by_card().await.map_err(db_error)?;
    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(fold_entities(rows, EntityKind::Card)))
}

/// GET /api/usage/experts — per-expert-session token totals + cost.
async fn list_expert_usage(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let rows = state.db.usage_rollup_by_expert().await.map_err(db_error)?;
    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(fold_entities(rows, EntityKind::Expert)))
}
