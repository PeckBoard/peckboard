//! Per-turn ("per-prompt") usage breakdown for one session.
//!
//! A turn is one `usage_events` row — the provider emits usage once, at end
//! of turn — so each row maps 1:1 to a prompt the agent answered. This module
//! re-correlates the raw event log against those turn boundaries (the same
//! `event.ts <= turn.ts` rule as [`super::operations`]) to attach to each
//! turn: the user prompt that started it, the files the agent `Read` during
//! it (the source of that turn's cache-read spend), and the files it edited.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use serde_json::Value;

use super::cost;
use crate::db::models::{Event, UsageEvent};
use crate::state::AppState;

/// Longest prompt snippet returned per turn. The full text lives in the
/// event log; this is a label, not a transcript.
const PROMPT_SNIPPET_CHARS: usize = 280;

/// One turn of a session: the usage row's token slices priced through the
/// shared cost model, plus what happened inside the turn.
#[derive(Debug, Clone, Serialize)]
pub struct TurnUsage {
    pub turn_seq: Option<i32>,
    /// End-of-turn timestamp (epoch ms) — the turn boundary.
    pub ts: i64,
    pub model: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub total_tokens: i64,
    /// Context-window occupancy at the end of this turn.
    pub context_tokens: i64,
    pub est_cost: f64,
    /// Snippet of the user prompt that started the turn, when one exists
    /// (worker kickoffs and tool-resume turns may have none).
    pub prompt: Option<String>,
    /// Distinct file paths the agent `Read` during the turn, in first-read
    /// order — what the turn's cache-read tokens were spent re-loading.
    pub files_read: Vec<String>,
    /// Distinct file paths edited (`Edit`/`Write`/`MultiEdit`) during the turn.
    pub files_edited: Vec<String>,
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

/// Strip an MCP tool's `mcp__<server>__` prefix; built-ins pass through.
fn tool_base_name(name: &str) -> &str {
    name.rsplit("__").next().unwrap_or(name)
}

/// The file path a read/edit tool call targets. Read uses `file_path` (the
/// mock provider's scenario uses `path`); notebook tools use `notebook_path`.
fn tool_file_path(input: &Value) -> Option<&str> {
    ["file_path", "path", "notebook_path"]
        .iter()
        .find_map(|k| input.get(k).and_then(|v| v.as_str()))
}

fn push_unique(list: &mut Vec<String>, path: &str) {
    if !list.iter().any(|p| p == path) {
        list.push(path.to_string());
    }
}

/// Build the per-turn breakdown from a session's usage rows + event log.
pub fn build_turns(usage: Vec<UsageEvent>, events: &[Event]) -> Vec<TurnUsage> {
    let mut sorted = usage;
    sorted.sort_by_key(|u| (u.ts, u.turn_seq));

    let mut turns: Vec<TurnUsage> = sorted
        .into_iter()
        .map(|u| TurnUsage {
            est_cost: cost::usage_cost(
                u.model.as_deref(),
                u.input_tokens,
                u.output_tokens,
                u.cache_read_tokens,
                u.cache_creation_tokens,
            ),
            turn_seq: u.turn_seq,
            ts: u.ts,
            model: u.model,
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_read_tokens: u.cache_read_tokens,
            cache_creation_tokens: u.cache_creation_tokens,
            total_tokens: u.total_tokens,
            context_tokens: u.context_tokens,
            prompt: None,
            files_read: Vec::new(),
            files_edited: Vec::new(),
        })
        .collect();

    for ev in events {
        match ev.kind.as_str() {
            "user" => {
                let Some(idx) = turn_index_ts(&turns, ev.ts) else {
                    continue;
                };
                let turn = &mut turns[idx];
                // First prompt wins: a turn answers the prompt that opened it.
                if turn.prompt.is_none()
                    && let Ok(data) = serde_json::from_str::<Value>(&ev.data)
                    && let Some(text) = data.get("text").and_then(|v| v.as_str())
                {
                    turn.prompt = Some(truncate_chars(text, PROMPT_SNIPPET_CHARS));
                }
            }
            "agent-tool-start" => {
                let Ok(data) = serde_json::from_str::<Value>(&ev.data) else {
                    continue;
                };
                let name = data.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let base = tool_base_name(name);
                let is_read = matches!(base, "Read" | "NotebookRead");
                let is_edit = matches!(base, "Edit" | "Write" | "MultiEdit" | "NotebookEdit");
                if !is_read && !is_edit {
                    continue;
                }
                let Some(path) = data.get("input").and_then(tool_file_path) else {
                    continue;
                };
                let Some(idx) = turn_index_ts(&turns, ev.ts) else {
                    continue;
                };
                let turn = &mut turns[idx];
                if is_read {
                    push_unique(&mut turn.files_read, path);
                } else {
                    push_unique(&mut turn.files_edited, path);
                }
            }
            _ => {}
        }
    }

    turns
}

/// `turn_index` over already-built [`TurnUsage`] rows.
fn turn_index_ts(turns: &[TurnUsage], ts: i64) -> Option<usize> {
    turns.iter().position(|t| t.ts >= ts)
}

/// GET /api/usage/sessions/:id/turns — oldest-first per-turn breakdown.
/// Empty list (not 404) for a known session with no usage; 404 only when the
/// session id is unknown.
pub async fn get_session_turns(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    match state.db.get_session(&id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "session not found" })),
            )
                .into_response();
        }
        Err(e) => return db_error(e),
    }
    let usage = match state.db.usage_events_for_session(&id).await {
        Ok(u) => u,
        Err(e) => return db_error(e),
    };
    let events = match state.db.list_events_by_session(&id, None).await {
        Ok(ev) => ev,
        Err(e) => return db_error(e),
    };
    Json(build_turns(usage, &events)).into_response()
}

fn db_error(e: anyhow::Error) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": e.to_string() })),
    )
        .into_response()
}
