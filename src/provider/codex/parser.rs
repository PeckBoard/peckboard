//! Codex CLI `--json` stream → [`ProviderEvent`].
//!
//! Shapes come from `CLI.md` / `fixtures/hello.jsonl` (synthetic capture of
//! the `openai/codex` exec JSONL schema). One JSON object per stdout line;
//! tag field is `type`.

use std::collections::HashSet;

use crate::provider::stream::ProviderEvent;
use crate::todo::{TodoItem, TodoStatus};

/// Per-turn parser state the harness wraps in [`super::CodexStream`].
#[derive(Default)]
pub(super) struct TurnState {
    pub conversation_id: Option<String>,
    pub error: Option<String>,
    completed: bool,
    started_tools: HashSet<String>,
    emitted_text: HashSet<String>,
}

/// Translate one JSONL object. Unknown types are ignored. `model` is the
/// session's model id, stamped on the usage event so it can be priced
/// (an unstamped row falls back to the default Opus rates).
pub(super) fn parse_stream_json(
    json: &serde_json::Value,
    state: &mut TurnState,
    model: Option<&str>,
) -> Vec<ProviderEvent> {
    let mut events = Vec::new();
    let msg_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match msg_type {
        "thread.started" => {
            if let Some(id) = json.get("thread_id").and_then(|v| v.as_str())
                && !id.is_empty()
            {
                state.conversation_id = Some(id.to_string());
            }
        }
        "turn.started" => {}
        "turn.completed" => {
            state.completed = true;
            if let Some(usage) = json.get("usage")
                && let Some(ev) = usage_event(usage, model)
            {
                events.push(ev);
            }
        }
        "turn.failed" => {
            if let Some(msg) = failed_message(json) {
                remember_error(state, msg);
            }
        }
        "error" => {
            if let Some(msg) = json.get("message").and_then(|v| v.as_str()) {
                remember_error(state, msg.to_string());
            }
        }
        "item.started" | "item.updated" | "item.completed" => {
            if let Some(item) = json.get("item") {
                events.extend(parse_item(msg_type, item, state));
            }
        }
        _ => {}
    }

    events
}

fn remember_error(state: &mut TurnState, msg: String) {
    if msg.trim().is_empty() {
        return;
    }
    // Transient reconnect chatter is non-fatal (turn continues).
    if msg.starts_with("Reconnecting") {
        return;
    }
    // A happy-path turn already closed at `turn.completed`; trailing
    // error/fail lines in the fixture exist only so isolated parser tests
    // can cover those shapes.
    if state.completed {
        return;
    }
    state.error = Some(msg);
}

fn failed_message(json: &serde_json::Value) -> Option<String> {
    json.get("error")
        .and_then(|e| e.get("message").or(Some(e)))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn parse_item(frame: &str, item: &serde_json::Value, state: &mut TurnState) -> Vec<ProviderEvent> {
    let kind = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let id = item
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or(kind)
        .to_string();
    match kind {
        "agent_message" => push_unique_text(&id, item, state, false),
        "reasoning" => push_unique_text(&id, item, state, true),
        "command_execution" => command_events(frame, &id, item, state),
        "mcp_tool_call" | "collab_tool_call" => mcp_events(frame, &id, item, state),
        "file_change" if frame == "item.completed" => file_change_events(item),
        "web_search" if frame == "item.completed" => web_search_events(&id, item),
        "todo_list" => todo_events(item),
        "error" => {
            if let Some(msg) = item.get("message").and_then(|v| v.as_str())
                && !msg.is_empty()
            {
                return vec![ProviderEvent::System {
                    text: msg.to_string(),
                    subtype: "warning".into(),
                    detail: item.clone(),
                }];
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn push_unique_text(
    id: &str,
    item: &serde_json::Value,
    state: &mut TurnState,
    thinking: bool,
) -> Vec<ProviderEvent> {
    let Some(text) = item.get("text").and_then(|v| v.as_str()) else {
        return Vec::new();
    };
    if text.is_empty() || !state.emitted_text.insert(id.to_string()) {
        return Vec::new();
    }
    if thinking {
        vec![ProviderEvent::Thinking {
            text: text.to_string(),
        }]
    } else {
        vec![ProviderEvent::Text {
            text: text.to_string(),
        }]
    }
}

fn command_events(
    frame: &str,
    id: &str,
    item: &serde_json::Value,
    state: &mut TurnState,
) -> Vec<ProviderEvent> {
    let command = item
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut events = Vec::new();
    if (frame == "item.started" || !state.started_tools.contains(id))
        && state.started_tools.insert(id.to_string())
    {
        events.push(ProviderEvent::ToolStart {
            tool_use_id: id.to_string(),
            name: "command_execution".into(),
            input: serde_json::json!({ "command": command }),
        });
    }
    if frame == "item.completed" {
        let output = item
            .get("aggregated_output")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty());
        let status = item.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let exit = item.get("exit_code").and_then(|v| v.as_i64());
        let failed = status == "failed" || status == "declined" || exit.is_some_and(|c| c != 0);
        let error = if failed {
            Some(output.clone().unwrap_or_else(|| {
                exit.map(|c| format!("exit {c}"))
                    .unwrap_or_else(|| status.to_string())
            }))
        } else {
            None
        };
        events.push(ProviderEvent::ToolEnd {
            tool_use_id: id.to_string(),
            output: if failed { None } else { output },
            error,
            images: Vec::new(),
        });
    }
    events
}

fn mcp_events(
    frame: &str,
    id: &str,
    item: &serde_json::Value,
    state: &mut TurnState,
) -> Vec<ProviderEvent> {
    let server = item.get("server").and_then(|v| v.as_str()).unwrap_or("");
    let tool = item.get("tool").and_then(|v| v.as_str()).unwrap_or("tool");
    let name = if server.is_empty() {
        tool.to_string()
    } else {
        format!("mcp__{server}__{tool}")
    };
    let input = item
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let mut events = Vec::new();
    if (frame == "item.started" || !state.started_tools.contains(id))
        && state.started_tools.insert(id.to_string())
    {
        events.push(ProviderEvent::ToolStart {
            tool_use_id: id.to_string(),
            name,
            input,
        });
    }
    if frame == "item.completed" {
        let status = item.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let err_msg = item
            .get("error")
            .and_then(|v| {
                v.as_str().map(str::to_string).or_else(|| {
                    v.get("message")
                        .and_then(|m| m.as_str())
                        .map(str::to_string)
                })
            })
            .filter(|s| !s.is_empty());
        let output = item.get("result").map(|v| match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        });
        let failed = status == "failed" || err_msg.is_some();
        events.push(ProviderEvent::ToolEnd {
            tool_use_id: id.to_string(),
            output: if failed { None } else { output },
            error: err_msg.or_else(|| failed.then(|| status.to_string())),
            images: Vec::new(),
        });
    }
    events
}

fn file_change_events(item: &serde_json::Value) -> Vec<ProviderEvent> {
    let Some(changes) = item.get("changes").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    changes
        .iter()
        .filter_map(|c| {
            let path = c.get("path").and_then(|v| v.as_str())?;
            if path.is_empty() {
                return None;
            }
            let kind = c.get("kind").and_then(|v| v.as_str()).unwrap_or("update");
            Some(ProviderEvent::FileDiff {
                path: path.to_string(),
                diff: String::new(),
                added: i64::from(kind == "add"),
                removed: i64::from(kind == "delete"),
                created: kind == "add",
            })
        })
        .collect()
}

fn web_search_events(id: &str, item: &serde_json::Value) -> Vec<ProviderEvent> {
    let query = item
        .get("query")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    vec![
        ProviderEvent::ToolStart {
            tool_use_id: id.to_string(),
            name: "web_search".into(),
            input: serde_json::json!({ "query": query }),
        },
        ProviderEvent::ToolEnd {
            tool_use_id: id.to_string(),
            output: query.as_str().map(str::to_string),
            error: None,
            images: Vec::new(),
        },
    ]
}

fn todo_events(item: &serde_json::Value) -> Vec<ProviderEvent> {
    let Some(items) = item.get("items").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let todos = items
        .iter()
        .filter_map(|t| {
            let content = t.get("text").and_then(|v| v.as_str())?;
            if content.is_empty() {
                return None;
            }
            let done = t
                .get("completed")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            Some(TodoItem {
                content: content.to_string(),
                status: if done {
                    TodoStatus::Done
                } else {
                    TodoStatus::Pending
                },
                active_form: None,
            })
        })
        .collect();
    vec![ProviderEvent::Todo { todos }]
}

/// Map `turn.completed.usage` onto Peckboard's four **disjoint** billed
/// slices.
///
/// Codex follows the OpenAI Responses API convention: `cached_input_tokens`
/// is a **subset** of `input_tokens`, not a sibling of it (see
/// openai/codex#16213 — `input_tokens: 18080`, `cached_input_tokens: 9728`,
/// and `/status` occupancy reported as `input + output`). Peckboard's
/// columns are disjoint (Claude/Grok semantics), so the cached slice has to
/// be *subtracted* out of the input slice before storing — adding them
/// double-counts occupancy (~2x gauge) and bills `input_rate` a second time
/// on every cached token.
///
/// ```text
/// input_tokens          = input - cached   (non-cached prompt)
/// cache_read_tokens     = cached_input_tokens
/// cache_creation_tokens = cache_write_input_tokens
/// context_tokens        = input_tokens as reported (the whole prompt)
/// ```
fn usage_event(usage: &serde_json::Value, model: Option<&str>) -> Option<ProviderEvent> {
    let num = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| usage.get(*k).and_then(|v| v.as_i64()))
            .unwrap_or(0)
    };
    let prompt_tokens = num(&["input_tokens"]);
    let cache_read_tokens = num(&["cached_input_tokens"]);
    let cache_creation_tokens = num(&["cache_write_input_tokens"]);
    let output_tokens = num(&["output_tokens"]) + num(&["reasoning_output_tokens"]);
    if prompt_tokens == 0
        && output_tokens == 0
        && cache_read_tokens == 0
        && cache_creation_tokens == 0
    {
        return None;
    }
    // Clamped: a provider build that ever reports the two as siblings would
    // otherwise store a negative input slice.
    let input_tokens = (prompt_tokens - cache_read_tokens).max(0);
    let context_tokens = prompt_tokens;
    Some(ProviderEvent::Usage {
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_creation_tokens,
        total_tokens: context_tokens + cache_creation_tokens + output_tokens,
        context_tokens,
        model: model.map(str::to_string),
        turn_seq: None,
    })
}

/// Parse `codex debug models [--bundled]` stdout into bare model slugs.
pub(super) fn parse_cli_models(output: &str) -> Option<Vec<String>> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        let ids = extract_model_ids(&value);
        if !ids.is_empty() {
            return Some(ids);
        }
    }
    // Some builds print one JSON object per line.
    let mut ids = Vec::new();
    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
            ids.extend(extract_model_ids(&value));
        } else if looks_like_slug(line) {
            ids.push(strip_codex_prefix(line).to_string());
        }
    }
    ids.retain(|s| !s.is_empty());
    if ids.is_empty() {
        None
    } else {
        Some(dedup(ids))
    }
}

fn extract_model_ids(value: &serde_json::Value) -> Vec<String> {
    let array = if let Some(arr) = value.as_array() {
        Some(arr.to_vec())
    } else if let Some(s) = value.as_str() {
        let s = strip_codex_prefix(s.trim());
        return if s.is_empty() {
            Vec::new()
        } else {
            vec![s.to_string()]
        };
    } else {
        value
            .get("models")
            .or_else(|| value.get("data"))
            .and_then(|v| v.as_array())
            .map(|a| a.to_vec())
    };
    let Some(array) = array else {
        // A single object with a slug/id.
        if let Some(id) = object_slug(value) {
            return vec![id];
        }
        return Vec::new();
    };
    let mut ids = Vec::new();
    for item in &array {
        if let Some(id) = item.as_str() {
            let id = strip_codex_prefix(id.trim());
            if !id.is_empty() {
                ids.push(id.to_string());
            }
        } else if let Some(id) = object_slug(item) {
            ids.push(id);
        }
    }
    dedup(ids)
}

fn object_slug(item: &serde_json::Value) -> Option<String> {
    item.get("slug")
        .or_else(|| item.get("id"))
        .or_else(|| item.get("name"))
        .or_else(|| item.get("model"))
        .and_then(|v| v.as_str())
        .map(|s| strip_codex_prefix(s.trim()).to_string())
        .filter(|s| !s.is_empty())
}

fn strip_codex_prefix(id: &str) -> &str {
    id.strip_prefix("codex:").unwrap_or(id)
}

fn looks_like_slug(line: &str) -> bool {
    !line.contains(' ')
        && line
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_' || c == ':')
}

fn dedup(ids: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    ids.into_iter()
        .filter(|id| seen.insert(id.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_line(line: &str, state: &mut TurnState) -> Vec<ProviderEvent> {
        let json: serde_json::Value = serde_json::from_str(line).unwrap();
        parse_stream_json(&json, state, Some("gpt-5.6-luna"))
    }

    fn parse_fixture() -> (TurnState, Vec<ProviderEvent>) {
        let mut state = TurnState::default();
        let mut events = Vec::new();
        for line in include_str!("fixtures/hello.jsonl").lines() {
            if line.trim().is_empty() {
                continue;
            }
            events.extend(parse_line(line, &mut state));
        }
        (state, events)
    }

    #[test]
    fn hello_fixture_maps_thread_thinking_tool_text_and_usage() {
        let (state, events) = parse_fixture();
        assert_eq!(
            state.conversation_id.as_deref(),
            Some("0199a213-81c0-7800-8aa1-bbab2a035a53")
        );
        // Happy path closed at turn.completed; trailing auth-fail lines in
        // the fixture must not rewrite a finished turn into a crash.
        assert!(state.error.is_none());

        let thinking = events.iter().find_map(|e| match e {
            ProviderEvent::Thinking { text } => Some(text.as_str()),
            _ => None,
        });
        assert_eq!(thinking, Some("**Replying with the single word OK**"));

        let starts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::ToolStart {
                    tool_use_id,
                    name,
                    input,
                } => Some((tool_use_id.as_str(), name.as_str(), input.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0].0, "item_1");
        assert_eq!(starts[0].1, "command_execution");
        assert_eq!(starts[0].2["command"], "bash -lc 'printf OK'");

        let ends: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::ToolEnd {
                    tool_use_id,
                    output,
                    error,
                    ..
                } => Some((tool_use_id.as_str(), output.clone(), error.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0].0, "item_1");
        assert_eq!(ends[0].1.as_deref(), Some("OK"));
        assert!(ends[0].2.is_none());

        let text = events.iter().find_map(|e| match e {
            ProviderEvent::Text { text } => Some(text.as_str()),
            _ => None,
        });
        assert_eq!(text, Some("OK"));

        // `cached_input_tokens` is a subset of `input_tokens`, so the stored
        // input slice is the non-cached remainder (24763 - 24448) and
        // occupancy is the reported prompt size, not their sum.
        let usage = events.iter().find_map(|e| match e {
            ProviderEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
                total_tokens,
                context_tokens,
                model,
                ..
            } => Some((
                *input_tokens,
                *output_tokens,
                *cache_read_tokens,
                *cache_creation_tokens,
                *total_tokens,
                *context_tokens,
                model.clone(),
            )),
            _ => None,
        });
        assert_eq!(
            usage,
            Some((
                315,
                24,
                24448,
                0,
                24787,
                24763,
                Some("gpt-5.6-luna".to_string())
            ))
        );
    }

    #[test]
    fn isolated_error_and_turn_failed_become_take_error() {
        let mut state = TurnState::default();
        parse_line(r#"{"type":"error","message":"Not logged in"}"#, &mut state);
        assert_eq!(state.error.as_deref(), Some("Not logged in"));

        let mut state = TurnState::default();
        parse_line(
            r#"{"type":"turn.failed","error":{"message":"Not logged in"}}"#,
            &mut state,
        );
        assert_eq!(state.error.as_deref(), Some("Not logged in"));
    }

    #[test]
    fn reconnecting_error_is_ignored() {
        let mut state = TurnState::default();
        parse_line(
            r#"{"type":"error","message":"Reconnecting... 1/5"}"#,
            &mut state,
        );
        assert!(state.error.is_none());
    }

    #[test]
    fn file_change_becomes_file_diff() {
        let mut state = TurnState::default();
        let events = parse_line(
            r#"{"type":"item.completed","item":{"id":"item_2","type":"file_change","changes":[{"path":"README.md","kind":"update"},{"path":"new.rs","kind":"add"}],"status":"completed"}}"#,
            &mut state,
        );
        match &events[0] {
            ProviderEvent::FileDiff {
                path,
                created,
                added,
                removed,
                ..
            } => {
                assert_eq!(path, "README.md");
                assert!(!*created);
                assert_eq!(*added, 0);
                assert_eq!(*removed, 0);
            }
            other => panic!("expected FileDiff, got {other:?}"),
        }
        match &events[1] {
            ProviderEvent::FileDiff {
                path,
                created,
                added,
                ..
            } => {
                assert_eq!(path, "new.rs");
                assert!(*created);
                assert_eq!(*added, 1);
            }
            other => panic!("expected FileDiff, got {other:?}"),
        }
    }

    #[test]
    fn mcp_tool_call_unwraps_to_qualified_name() {
        let mut state = TurnState::default();
        let started = parse_line(
            r#"{"type":"item.started","item":{"id":"item_5","type":"mcp_tool_call","server":"docs","tool":"search","arguments":{"q":"exec"},"status":"in_progress"}}"#,
            &mut state,
        );
        match &started[0] {
            ProviderEvent::ToolStart { name, input, .. } => {
                assert_eq!(name, "mcp__docs__search");
                assert_eq!(input["q"], "exec");
            }
            other => panic!("expected ToolStart, got {other:?}"),
        }
        let ended = parse_line(
            r#"{"type":"item.completed","item":{"id":"item_5","type":"mcp_tool_call","server":"docs","tool":"search","arguments":{"q":"exec"},"status":"completed"}}"#,
            &mut state,
        );
        match &ended[0] {
            ProviderEvent::ToolEnd {
                tool_use_id, error, ..
            } => {
                assert_eq!(tool_use_id, "item_5");
                assert!(error.is_none());
            }
            other => panic!("expected ToolEnd, got {other:?}"),
        }
    }

    #[test]
    fn parse_cli_models_json_slugs_and_wrappers() {
        assert_eq!(
            parse_cli_models(r#"["gpt-5.6-luna","gpt-6-astra"]"#).unwrap(),
            vec!["gpt-5.6-luna", "gpt-6-astra"]
        );
        assert_eq!(
            parse_cli_models(r#"{"models":[{"slug":"gpt-5.6-terra"},{"id":"codex:gpt-5.6-sol"}]}"#)
                .unwrap(),
            vec!["gpt-5.6-terra", "gpt-5.6-sol"]
        );
        assert!(parse_cli_models("").is_none());
        assert!(parse_cli_models("not json and not a slug line because spaces").is_none());
    }
}
