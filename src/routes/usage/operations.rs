//! Per-operation cost derivation for the usage dashboard.
//!
//! Turns the raw event log + per-turn `usage_events` into [`OperationCost`]
//! lists for three operation kinds:
//!
//! - **file_update** — each `Edit`/`Write` (and `MultiEdit`) tool call,
//!   grouped by `file_path`. The cost of the turn that contained the edit is
//!   attributed to the file; when one turn edits several distinct files its
//!   cost is split evenly across them so a single turn is never counted twice.
//! - **ask_expert** — each `ask_expert` consultation: the asking turn plus the
//!   expert's answering turn, correlated by the reply's `reply_to_session_id`
//!   (the structured linkage the reply-mode tool call carries).
//! - **qa** — each `question` paired with its `question-resolved` (the
//!   `ask_user` / PM escalation flow), spanning the asking turn and the turn
//!   the answer resumed.
//!
//! A "turn" is one `usage_events` row (the provider emits usage once per turn,
//! at end of turn — so a turn's usage `ts` is `>=` the `ts` of every tool /
//! question event inside it, and `<` the next turn's events). An event at
//! `ts` therefore belongs to the first turn whose `ts >= event.ts`. Every turn
//! is priced through the shared [`cost::usage_cost`] so the numbers match the
//! rest of the usage dashboard exactly.
//!
//! The derivation functions are `pub` so sibling endpoints (e.g. usage trends)
//! can bucket the same per-operation costs over time without duplicating the
//! event-parsing logic.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::Value;

use super::cost;
use super::{OperationCost, OperationKind};
use crate::db::Db;
use crate::db::models::{Event, Session, UsageEvent};
use crate::state::AppState;

/// What set of sessions an operation-cost query covers.
#[derive(Debug, Clone)]
pub enum OperationScope {
    /// A single session.
    Session(String),
    /// Every session owned by a project.
    Project(String),
    /// Every session in the install.
    Global,
}

/// One priced turn — a single `usage_events` row reduced to the numbers the
/// operation attribution needs: a representative token count, the USD cost
/// (computed once, via the shared cost model), and the cache-read slice on
/// its own (what the file_read attribution prices).
struct Turn {
    ts: i64,
    tokens: i64,
    cost: f64,
    cache_read_tokens: i64,
    cache_read_cost: f64,
}

fn build_turns(usage: &[UsageEvent]) -> Vec<Turn> {
    let mut turns: Vec<Turn> = usage
        .iter()
        .map(|u| {
            let billed =
                u.input_tokens + u.output_tokens + u.cache_read_tokens + u.cache_creation_tokens;
            Turn {
                ts: u.ts,
                // `total_tokens` is the provider's roll-up; fall back to the
                // billed slices when it wasn't reported.
                tokens: if u.total_tokens > 0 {
                    u.total_tokens
                } else {
                    billed
                },
                cost: cost::usage_cost(
                    u.model.as_deref(),
                    u.input_tokens,
                    u.output_tokens,
                    u.cache_read_tokens,
                    u.cache_creation_tokens,
                ),
                cache_read_tokens: u.cache_read_tokens,
                cache_read_cost: cost::usage_cost(u.model.as_deref(), 0, 0, u.cache_read_tokens, 0),
            }
        })
        .collect();
    turns.sort_by_key(|t| t.ts);
    turns
}

/// The turn an event at `ts` belongs to: the first turn whose usage `ts` is at
/// or after the event (usage is emitted at end of turn). `turns` is ascending.
fn containing_turn(turns: &[Turn], ts: i64) -> Option<&Turn> {
    turns.iter().find(|t| t.ts >= ts)
}

/// The first turn strictly after `ts` — used for the turn an answer resumed.
fn turn_after(turns: &[Turn], ts: i64) -> Option<&Turn> {
    turns.iter().find(|t| t.ts > ts)
}

/// Strip an MCP tool's `mcp__<server>__` prefix so `mcp__peckboard__ask_expert`
/// matches `ask_expert`; built-in tools like `Edit` pass through unchanged.
fn tool_base_name(name: &str) -> &str {
    name.rsplit("__").next().unwrap_or(name)
}

fn event_data(ev: &Event) -> Value {
    serde_json::from_str(&ev.data).unwrap_or(Value::Null)
}

type SessionData = (Session, Vec<Event>, Vec<Turn>);

/// The file path a read/edit tool call targets. Edits use `file_path`; Read
/// variants may use `path`, and notebook tools `notebook_path`.
fn tool_file_path(input: &Value) -> Option<&str> {
    ["file_path", "path", "notebook_path"]
        .iter()
        .find_map(|k| input.get(k).and_then(|v| v.as_str()))
}

/// Shared per-file turn-cost attribution: each turn that contained a matching
/// tool call is attributed to the file(s) it touched, splitting the turn's
/// value evenly across distinct files so one turn is never counted twice.
/// `turn_value` selects which (tokens, cost) slice of the turn to attribute.
fn file_costs(
    sessions: &[SessionData],
    kind: OperationKind,
    matches_tool: fn(&str) -> bool,
    turn_value: fn(&Turn) -> (i64, f64),
) -> Vec<OperationCost> {
    #[derive(Default)]
    struct Acc {
        tokens: f64,
        cost: f64,
        ts: i64,
    }
    let mut acc: BTreeMap<String, Acc> = BTreeMap::new();

    for (_s, events, turns) in sessions {
        // turn index -> distinct file paths touched in that turn.
        let mut turn_files: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();
        let mut path_ts: HashMap<String, i64> = HashMap::new();

        for ev in events {
            if ev.kind != "agent-tool-start" {
                continue;
            }
            let data = event_data(ev);
            let name = data.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if !matches_tool(tool_base_name(name)) {
                continue;
            }
            let Some(path) = data.get("input").and_then(tool_file_path) else {
                continue;
            };
            let Some(idx) = turns.iter().position(|t| t.ts >= ev.ts) else {
                continue;
            };
            turn_files.entry(idx).or_default().insert(path.to_string());
            let slot = path_ts.entry(path.to_string()).or_insert(ev.ts);
            if ev.ts > *slot {
                *slot = ev.ts;
            }
        }

        for (idx, files) in turn_files {
            let turn = &turns[idx];
            let (turn_tokens, turn_cost) = turn_value(turn);
            let n = files.len() as f64;
            for f in files {
                let ts = path_ts.get(&f).copied().unwrap_or(turn.ts);
                let entry = acc.entry(f).or_default();
                entry.tokens += turn_tokens as f64 / n;
                entry.cost += turn_cost / n;
                if ts > entry.ts {
                    entry.ts = ts;
                }
            }
        }
    }

    acc.into_iter()
        .map(|(path, a)| OperationCost {
            kind,
            ref_id: path.clone(),
            label: path,
            tokens: a.tokens.round() as i64,
            est_cost: a.cost,
            ts: a.ts,
        })
        .collect()
}

/// file_update: attribute each turn that contained an `Edit`/`Write`/`MultiEdit`
/// to the file(s) it touched, splitting evenly when a turn edits several files.
fn file_update_costs(sessions: &[SessionData]) -> Vec<OperationCost> {
    file_costs(
        sessions,
        OperationKind::FileUpdate,
        |name| matches!(name, "Edit" | "Write" | "MultiEdit" | "NotebookEdit"),
        |t| (t.tokens, t.cost),
    )
}

/// file_read: attribute each turn's *cache-read* slice to the file(s) the
/// agent `Read` in that turn — the cost of re-loading previously-cached
/// context, broken down by what was actually read.
fn file_read_costs(sessions: &[SessionData]) -> Vec<OperationCost> {
    file_costs(
        sessions,
        OperationKind::FileRead,
        |name| matches!(name, "Read" | "NotebookRead"),
        |t| (t.cache_read_tokens, t.cache_read_cost),
    )
}

/// ask_expert: one OperationCost per consultation. The asking turn (the turn
/// that called `ask_expert` in ask mode) plus the expert's answering turn (the
/// turn that called `ask_expert` in reply mode), linked by the reply's
/// `reply_to_session_id` pointing back at the asking session.
fn ask_expert_costs(sessions: &[SessionData]) -> Vec<OperationCost> {
    struct Ask {
        session: String,
        ts: i64,
        ref_id: String,
        label: String,
        tokens: i64,
        cost: f64,
    }
    struct Reply {
        reply_to: String,
        ts: i64,
        tokens: i64,
        cost: f64,
    }

    let mut asks: Vec<Ask> = Vec::new();
    let mut replies: Vec<Reply> = Vec::new();

    for (s, events, turns) in sessions {
        for ev in events {
            if ev.kind != "agent-tool-start" {
                continue;
            }
            let data = event_data(ev);
            let name = data.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if tool_base_name(name) != "ask_expert" {
                continue;
            }
            let Some(turn) = containing_turn(turns, ev.ts) else {
                continue;
            };
            let input = data.get("input").cloned().unwrap_or(Value::Null);

            if let Some(reply_to) = input.get("reply_to_session_id").and_then(|v| v.as_str()) {
                replies.push(Reply {
                    reply_to: reply_to.to_string(),
                    ts: ev.ts,
                    tokens: turn.tokens,
                    cost: turn.cost,
                });
            } else {
                let tool_use_id = data
                    .get("toolUseId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let label = input
                    .get("expert_id")
                    .and_then(|v| v.as_str())
                    .or_else(|| input.get("area").and_then(|v| v.as_str()))
                    .unwrap_or("ask_expert")
                    .to_string();
                let ref_id = if tool_use_id.is_empty() {
                    format!("ask_expert:{}:{}", s.id, ev.ts)
                } else {
                    tool_use_id
                };
                asks.push(Ask {
                    session: s.id.clone(),
                    ts: ev.ts,
                    ref_id,
                    label,
                    tokens: turn.tokens,
                    cost: turn.cost,
                });
            }
        }
    }

    asks.sort_by_key(|a| a.ts);
    replies.sort_by_key(|r| r.ts);
    let mut consumed = vec![false; replies.len()];
    let mut out = Vec::with_capacity(asks.len());

    for ask in &asks {
        let mut tokens = ask.tokens;
        let mut cost = ask.cost;
        // Earliest unconsumed reply that points back at this asker and lands
        // at/after the ask. `replies` is sorted by ts, so the first match wins.
        for (j, r) in replies.iter().enumerate() {
            if !consumed[j] && r.reply_to == ask.session && r.ts >= ask.ts {
                consumed[j] = true;
                tokens += r.tokens;
                cost += r.cost;
                break;
            }
        }
        out.push(OperationCost {
            kind: OperationKind::AskExpert,
            ref_id: ask.ref_id.clone(),
            label: ask.label.clone(),
            tokens,
            est_cost: cost,
            ts: ask.ts,
        });
    }

    out
}

/// The first question's text from a `question` event's data, handling both the
/// `ask_user` MCP shape (`data.questions[]`) and the control-request shape
/// (`data.payload.questions[]`).
fn first_question_text(data: &Value) -> Option<String> {
    let pick = |questions: &Value| -> Option<String> {
        questions
            .as_array()?
            .first()?
            .get("question")?
            .as_str()
            .map(|s| s.to_string())
    };
    if let Some(q) = data.get("questions").and_then(pick) {
        return Some(q);
    }
    data.get("payload")
        .and_then(|p| p.get("questions"))
        .and_then(pick)
}

/// qa: one OperationCost per `question` that has a matching `question-resolved`,
/// spanning the asking turn and the turn the answer resumed.
fn qa_costs(sessions: &[SessionData]) -> Vec<OperationCost> {
    let mut out = Vec::new();

    for (_s, events, turns) in sessions {
        let mut questions: Vec<(String, i64, String)> = Vec::new();
        let mut resolved: HashMap<String, i64> = HashMap::new();

        for ev in events {
            match ev.kind.as_str() {
                "question" => {
                    let data = event_data(ev);
                    let label =
                        first_question_text(&data).unwrap_or_else(|| "question".to_string());
                    questions.push((ev.id.clone(), ev.ts, label));
                }
                "question-resolved" => {
                    let data = event_data(ev);
                    if let Some(qid) = data
                        .get("question_id")
                        .or_else(|| data.get("questionId"))
                        .and_then(|v| v.as_str())
                    {
                        resolved.entry(qid.to_string()).or_insert(ev.ts);
                    }
                }
                _ => {}
            }
        }

        for (id, qts, label) in questions {
            let Some(&rts) = resolved.get(&id) else {
                continue;
            };
            let ask_turn = containing_turn(turns, qts);
            let answer_turn = turn_after(turns, rts);

            let mut tokens = 0i64;
            let mut cost = 0.0;
            if let Some(t) = ask_turn {
                tokens += t.tokens;
                cost += t.cost;
            }
            // Add the answer turn unless it's the same turn already counted.
            if let Some(t) = answer_turn
                && ask_turn.map(|a| a.ts) != Some(t.ts)
            {
                tokens += t.tokens;
                cost += t.cost;
            }

            out.push(OperationCost {
                kind: OperationKind::Qa,
                ref_id: id,
                label,
                tokens,
                est_cost: cost,
                ts: qts,
            });
        }
    }

    out
}

async fn load_scope(db: &Db, scope: &OperationScope) -> anyhow::Result<Vec<Session>> {
    Ok(match scope {
        OperationScope::Session(id) => db.get_session(id).await?.into_iter().collect(),
        OperationScope::Project(pid) => db
            .list_sessions()
            .await?
            .into_iter()
            .filter(|s| s.project_id.as_deref() == Some(pid.as_str()))
            .collect(),
        OperationScope::Global => db.list_sessions().await?,
    })
}

async fn load_session_data(db: &Db, sessions: Vec<Session>) -> anyhow::Result<Vec<SessionData>> {
    let mut out = Vec::with_capacity(sessions.len());
    for s in sessions {
        let events = db.list_events_by_session(&s.id, None).await?;
        let usage = db.usage_events_for_session(&s.id).await?;
        let turns = build_turns(&usage);
        out.push((s, events, turns));
    }
    Ok(out)
}

fn sort_ops(ops: &mut [OperationCost]) {
    ops.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.label.cmp(&b.label)));
}

/// Derive the per-operation costs of one `kind` within `scope`.
pub async fn operation_costs(
    db: &Db,
    kind: OperationKind,
    scope: &OperationScope,
) -> anyhow::Result<Vec<OperationCost>> {
    let data = load_session_data(db, load_scope(db, scope).await?).await?;
    let mut ops = match kind {
        OperationKind::FileUpdate => file_update_costs(&data),
        OperationKind::FileRead => file_read_costs(&data),
        OperationKind::AskExpert => ask_expert_costs(&data),
        OperationKind::Qa => qa_costs(&data),
    };
    sort_ops(&mut ops);
    Ok(ops)
}

/// All three operation kinds concatenated — convenient for callers (e.g. the
/// trends endpoint) that bucket every operation's cost over time.
pub async fn all_operation_costs(
    db: &Db,
    scope: &OperationScope,
) -> anyhow::Result<Vec<OperationCost>> {
    let data = load_session_data(db, load_scope(db, scope).await?).await?;
    let mut ops = file_update_costs(&data);
    ops.extend(file_read_costs(&data));
    ops.extend(ask_expert_costs(&data));
    ops.extend(qa_costs(&data));
    sort_ops(&mut ops);
    Ok(ops)
}

/// Query params for `GET /api/usage/operations`. `kind` is required; the scope
/// narrows to a session or project when given, else the whole install.
#[derive(Debug, Deserialize)]
pub struct OperationsQuery {
    pub kind: String,
    pub session_id: Option<String>,
    pub project_id: Option<String>,
}

/// `GET /api/usage/operations?kind=file_update|ask_expert|qa` — per-operation
/// cost list, optionally scoped by `session_id` or `project_id`. Auth-protected
/// by the usage router's `require_auth` layer.
pub async fn get_operations(
    State(state): State<Arc<AppState>>,
    Query(q): Query<OperationsQuery>,
) -> Response {
    let kind = match q.kind.as_str() {
        "file_update" => OperationKind::FileUpdate,
        "file_read" => OperationKind::FileRead,
        "ask_expert" => OperationKind::AskExpert,
        "qa" => OperationKind::Qa,
        other => {
            return (
                StatusCode::BAD_REQUEST,
                format!(
                    "unknown operation kind: {other} (expected file_update|file_read|ask_expert|qa)"
                ),
            )
                .into_response();
        }
    };
    let scope = match (q.session_id, q.project_id) {
        (Some(sid), _) => OperationScope::Session(sid),
        (None, Some(pid)) => OperationScope::Project(pid),
        (None, None) => OperationScope::Global,
    };

    match operation_costs(&state.db, kind, &scope).await {
        Ok(ops) => Json(ops).into_response(),
        Err(e) => {
            tracing::error!("usage operations derivation failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to derive operation costs",
            )
                .into_response()
        }
    }
}
