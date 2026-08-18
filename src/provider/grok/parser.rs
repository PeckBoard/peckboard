//! Parser for `grok -p --output-format streaming-json` output.
//!
//! Grok's headless streaming-json is newline-delimited, one flat JSON object
//! per line tagged with a `type` (verified against the shipped grok 1.0.3
//! binary end to end):
//!
//! ```json
//! {"type":"available_commands","tools":[...],"commands":[...]}
//! {"type":"thought","data":"Analyzing the directory structure..."}
//! {"type":"text","data":"Here's"}
//! {"type":"tool_call","toolCallId":"call_1","title":"Read","kind":"read",
//!  "status":"in_progress","toolName":"read_file","rawInput":{"path":"x"}}
//! {"type":"tool_call_update","toolCallId":"call_1","status":"completed",
//!  "content":[],"rawOutput":{"lines":42}}
//! {"type":"usage","usage":{"input_tokens":10,"output_tokens":5,...}}
//! {"type":"end","stopReason":"end_turn","sessionId":"...","requestId":"...",
//!  "usage":{...},"num_turns":1,"modelUsage":{"grok-4.5":{"inputTokens":10,...}}}
//! ```
//!
//! Pre-1.0 CLIs shaped tool frames as `{"type":"tool_call","name":…,
//! "input":…}` / `{"type":"tool","toolCallId":…,"result":…}`; both
//! generations parse.
//!
//! We translate each line into zero or more [`ProviderEvent`]s and carry the
//! `sessionId` out via `conversation_id` so the next turn can resume it with
//! `--resume`. The `Started` and final `Completed` events are emitted by
//! the run loop (which owns the model label and the captured id), so this
//! parser never produces them. `thought` (reasoning) maps to
//! [`ProviderEvent::Thinking`], the same collapsed channel the Claude
//! provider feeds from its `thinking` blocks.
//!
//! Token counts: grok 1.0 reports them on the `end` frame (flat `usage`
//! rollup plus a per-model `modelUsage` map), which [`end_usage_events`]
//! maps to [`ProviderEvent::Usage`]. The mid-stream `usage` frame carries
//! the same rollup and is deliberately ignored to avoid double-counting.
//! Earlier CLIs (0.2.x) reported no counts anywhere, so an `end` frame
//! without usage still parses fine.
//!
//! The exact tool-event shape isn't formally specified, so every accessor is
//! defensive: an unrecognised shape yields no events rather than an error.

use crate::provider::stream::ProviderEvent;
/// Parse one JSON line of grok streaming-json into provider events, updating
/// `conversation_id` from any `sessionId` the line carries (the `end` frame
/// always does).
pub(super) fn parse_stream_json(
    json: &serde_json::Value,
    conversation_id: &mut Option<String>,
) -> Vec<ProviderEvent> {
    let mut events = Vec::new();
    let msg_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");

    if let Some(cid) = extract_session_id(json) {
        *conversation_id = Some(cid);
    }

    match msg_type {
        // Streamed assistant text chunk.
        "text" => {
            if let Some(text) = event_data_str(json) {
                if !text.is_empty() {
                    events.push(ProviderEvent::Text { text });
                }
            }
        }

        // Reasoning / chain-of-thought — its own collapsed channel.
        "thought" => {
            if let Some(text) = event_data_str(json) {
                if !text.is_empty() {
                    events.push(ProviderEvent::Thinking { text });
                }
            }
        }

        // A tool invocation begins. grok 1.0 names the tool `toolName` and
        // its arguments `rawInput` (ACP leaf names); older CLIs used
        // `name`/`input` — both shapes parse.
        "tool_call" => {
            let tool_use_id = tool_id(json);
            let name = json
                .get("name")
                .or_else(|| json.get("tool"))
                .or_else(|| json.get("toolName"))
                .and_then(|v| v.as_str())
                .unwrap_or("tool")
                .to_string();
            let input = json
                .get("rawInput")
                .or_else(|| json.get("input"))
                .or_else(|| json.get("args"))
                .or_else(|| json.get("arguments"))
                .or_else(|| json.get("parameters"))
                .or_else(|| json.get("params"))
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            // Grok's headless `--always-approve` runs its built-in tools
            // autonomously with no pre-execution gate peckboard could hook,
            // so terminal calls are rendered honestly — real name, input,
            // and (in the result arm) real result. The old fake "denied"
            // row showed an error for a command that had actually executed;
            // the WORKING_STYLE prompt remains the steer away from the
            // internal shell.
            events.push(ProviderEvent::ToolStart {
                tool_use_id,
                name,
                input,
            });
        }

        // grok 1.0's tool progress/result frame. Only a terminal status
        // closes the row; in-flight updates would otherwise end it early.
        "tool_call_update" => {
            let status = json.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if status == "completed" || status == "failed" {
                let tool_use_id = tool_id(json);
                let output = tool_output(json);
                let (output, error) = if status == "failed" {
                    (None, output.or_else(|| Some("tool call failed".into())))
                } else {
                    (output, None)
                };
                events.push(ProviderEvent::ToolEnd {
                    tool_use_id,
                    output,
                    error,
                    images: Vec::new(),
                });
            }
        }

        // A tool finished, carrying its result (pre-1.0 CLIs).
        "tool" => {
            let tool_use_id = tool_id(json);
            let is_error = json
                .get("isError")
                .or_else(|| json.get("is_error"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let output = tool_output(json);
            let explicit_error = json
                .get("error")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .filter(|s| !s.is_empty());
            let (output, error) = match (is_error, explicit_error) {
                (_, Some(e)) => (None, Some(e)),
                (true, None) => (None, output),
                (false, None) => (output, None),
            };
            events.push(ProviderEvent::ToolEnd {
                tool_use_id,
                output,
                error,
                images: Vec::new(),
            });
        }

        // `end` also carries the session id (captured above) and the turn's
        // token rollup; the run loop emits the terminal Completed itself.
        "end" => events.extend(end_usage_events(json)),

        // The standalone `usage` frame duplicates the rollup the `end` frame
        // carries, so parsing both would double-count. `error` is inspected
        // by the run loop for a crash reason, and `available_commands` is
        // startup chatter — none produce events here.
        _ => {}
    }

    events
}

/// Token usage from an `end` frame (grok 1.0+). Prefers the per-model
/// `modelUsage` map (camelCase counters, one `Usage` per model so multi-model
/// turns roll up correctly); falls back to the flat `usage` rollup
/// (snake_case counters, no model). Same context/total convention as the
/// Claude and Cursor providers: context is the window at end of turn
/// (input + cache read + cache creation), total adds the generated output.
fn end_usage_events(json: &serde_json::Value) -> Vec<ProviderEvent> {
    let usage_event =
        |input: i64, output: i64, cache_read: i64, cache_creation: i64, model: Option<String>| {
            if input + output + cache_read + cache_creation == 0 {
                return None;
            }
            let context = input + cache_read + cache_creation;
            Some(ProviderEvent::Usage {
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: cache_read,
                cache_creation_tokens: cache_creation,
                total_tokens: context + output,
                context_tokens: context,
                model,
                turn_seq: None,
            })
        };

    if let Some(models) = json.get("modelUsage").and_then(|v| v.as_object())
        && !models.is_empty()
    {
        return models
            .iter()
            .filter_map(|(model, u)| {
                let count = |key: &str| u.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
                usage_event(
                    count("inputTokens"),
                    count("outputTokens"),
                    count("cacheReadInputTokens"),
                    count("cacheCreationInputTokens"),
                    Some(model.clone()),
                )
            })
            .collect();
    }

    let Some(u) = json.get("usage") else {
        return Vec::new();
    };
    let count = |key: &str| u.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
    usage_event(
        count("input_tokens"),
        count("output_tokens"),
        count("cache_read_input_tokens"),
        count("cache_creation_input_tokens"),
        None,
    )
    .into_iter()
    .collect()
}

/// Grok's `text` / `thought` payload field is `data`, but tolerate `text`.
fn event_data_str(json: &serde_json::Value) -> Option<String> {
    json.get("data")
        .or_else(|| json.get("text"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// A tool call/result id under any of the names grok might use. Falls back to
/// the tool name, then a constant, so a `ToolStart` and its `ToolEnd` still
/// pair up even when the id field is absent.
fn tool_id(json: &serde_json::Value) -> String {
    for key in ["toolCallId", "tool_call_id", "id", "callId"] {
        if let Some(s) = json.get(key).and_then(|v| v.as_str())
            && !s.is_empty()
        {
            return s.to_string();
        }
    }
    json.get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("tool")
        .to_string()
}

/// Pull a tool result's textual output, tolerating a few field names and a
/// non-string payload (serialised to JSON). grok 1.0's `tool_call_update`
/// carries `rawOutput` (arbitrary JSON) and/or `content` (ACP content-block
/// array); older CLIs used `result` / `output` / `data`.
fn tool_output(json: &serde_json::Value) -> Option<String> {
    let value = json
        .get("rawOutput")
        .or_else(|| json.get("result"))
        .or_else(|| json.get("output"))
        .or_else(|| json.get("content"))
        .or_else(|| json.get("data"))?;
    match value {
        serde_json::Value::String(s) if s.is_empty() => None,
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Null => None,
        serde_json::Value::Array(blocks) => acp_content_text(blocks),
        other => Some(other.to_string()),
    }
}

/// Join the text out of an ACP content-block array
/// (`[{"type":"content","content":{"type":"text","text":"..."}}]`),
/// tolerating bare `{"type":"text","text":"..."}` blocks. `None` when the
/// array is empty or carries no text.
fn acp_content_text(blocks: &[serde_json::Value]) -> Option<String> {
    let texts: Vec<&str> = blocks
        .iter()
        .filter_map(|block| {
            let inner = block.get("content").unwrap_or(block);
            inner.get("text").and_then(|v| v.as_str())
        })
        .filter(|s| !s.is_empty())
        .collect();
    if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n"))
    }
}

/// Pull grok's chat/session id out of a frame, tolerating `sessionId` /
/// `session_id`.
fn extract_session_id(json: &serde_json::Value) -> Option<String> {
    for key in ["sessionId", "session_id"] {
        if let Some(s) = json.get(key).and_then(|v| v.as_str())
            && !s.is_empty()
        {
            return Some(s.to_string());
        }
    }
    None
}

/// The crash reason carried by an `{"type":"error", ...}` line, if any.
pub(super) fn error_reason(json: &serde_json::Value) -> Option<String> {
    if json.get("type").and_then(|v| v.as_str()) != Some("error") {
        return None;
    }
    let msg = json
        .get("message")
        .or_else(|| json.get("error"))
        .or_else(|| json.get("data"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "grok reported an error".to_string());
    Some(msg)
}

/// Result of parsing `grok models` stdout. Catalog is auth-scoped: OAuth
/// hosts may list `grok-4.6` while API-key / unauth hosts only list
/// `grok-4.5`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ParsedCatalog {
    /// Id from the `Default model:` line, when present.
    pub default_id: Option<String>,
    /// Model ids in CLI order, with `default_id` moved to the front when it
    /// appeared in the list (or alone when the bullet list was empty).
    pub models: Vec<String>,
}

/// Parse the human-readable listing from `grok models`.
///
/// Verified against grok 1.0.3:
/// ```text
/// You are logged in with grok.com.
///
/// Default model: grok-4.6
///
/// Available models:
///   * grok-4.6 (default)
///   - grok-4.5
/// ```
/// Returns `None` when nothing usable was found so the caller can fall back
/// to the static seed.
pub(super) fn parse_cli_models(output: &str) -> Option<ParsedCatalog> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut default_id: Option<String> = None;
    let mut models: Vec<String> = Vec::new();
    let mut in_list = false;

    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line
            .strip_prefix("Default model:")
            .or_else(|| line.strip_prefix("default model:"))
        {
            let id = rest.trim().to_string();
            if !id.is_empty() {
                default_id = Some(id);
            }
            continue;
        }
        if line.eq_ignore_ascii_case("Available models:")
            || line.eq_ignore_ascii_case("Available models")
        {
            in_list = true;
            continue;
        }
        if !in_list {
            continue;
        }
        // Bullet lines: "* id (default)" / "- id" / "• id". Stop if we hit
        // prose that isn't a model bullet (e.g. a trailing tip).
        let bullet = line
            .strip_prefix('*')
            .or_else(|| line.strip_prefix('-'))
            .or_else(|| line.strip_prefix('•'))
            .map(str::trim);
        let Some(rest) = bullet else {
            // Non-bullet after the header — end of list.
            break;
        };
        let id = rest
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_end_matches(['(', ')', ','])
            .to_string();
        if id.is_empty() {
            continue;
        }
        if !models.iter().any(|existing| existing == &id) {
            models.push(id);
        }
    }

    // Prefer the explicit default even if the bullet list was missing it.
    if let Some(def) = default_id.as_ref() {
        if let Some(pos) = models.iter().position(|m| m == def) {
            if pos != 0 {
                let m = models.remove(pos);
                models.insert(0, m);
            }
        } else {
            models.insert(0, def.clone());
        }
    }

    if models.is_empty() {
        None
    } else {
        Some(ParsedCatalog { default_id, models })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: serde_json::Value, conv: &mut Option<String>) -> Vec<ProviderEvent> {
        parse_stream_json(&json, conv)
    }

    #[test]
    fn text_event_becomes_text() {
        let mut conv = None;
        let events = parse(serde_json::json!({"type":"text","data":"Hello"}), &mut conv);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ProviderEvent::Text { text } if text == "Hello"));
    }

    #[test]
    fn empty_text_is_dropped() {
        let mut conv = None;
        let events = parse(serde_json::json!({"type":"text","data":""}), &mut conv);
        assert!(events.is_empty());
    }

    #[test]
    fn thought_becomes_thinking() {
        let mut conv = None;
        let events = parse(
            serde_json::json!({"type":"thought","data":"hmm let me think"}),
            &mut conv,
        );
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], ProviderEvent::Thinking { text } if text == "hmm let me think")
        );
    }

    #[test]
    fn empty_thought_is_dropped() {
        let mut conv = None;
        let events = parse(serde_json::json!({"type":"thought","data":""}), &mut conv);
        assert!(events.is_empty());
    }

    #[test]
    fn tool_call_becomes_tool_start() {
        let mut conv = None;
        let events = parse(
            serde_json::json!({
                "type":"tool_call",
                "toolCallId":"tc1",
                "name":"read_file",
                "input":{"path":"src/main.rs"}
            }),
            &mut conv,
        );
        assert_eq!(events.len(), 1);
        let ProviderEvent::ToolStart {
            tool_use_id,
            name,
            input,
        } = &events[0]
        else {
            panic!("expected ToolStart, got {:?}", events[0]);
        };
        assert_eq!(tool_use_id, "tc1");
        assert_eq!(name, "read_file");
        assert_eq!(input["path"], "src/main.rs");
    }

    #[test]
    fn tool_call_falls_back_to_name_as_id_and_alt_input_keys() {
        let mut conv = None;
        let events = parse(
            serde_json::json!({"type":"tool_call","name":"grep","args":{"q":"TODO"}}),
            &mut conv,
        );
        let ProviderEvent::ToolStart {
            tool_use_id,
            name,
            input,
        } = &events[0]
        else {
            panic!("expected ToolStart");
        };
        // No id field → falls back to the tool name so the ToolEnd can pair.
        assert_eq!(tool_use_id, "grep");
        assert_eq!(name, "grep");
        assert_eq!(input["q"], "TODO");
    }

    #[test]
    fn tool_result_becomes_tool_end() {
        let mut conv = None;
        let events = parse(
            serde_json::json!({"type":"tool","toolCallId":"tc1","result":"file contents"}),
            &mut conv,
        );
        let ProviderEvent::ToolEnd {
            tool_use_id,
            output,
            error,
            ..
        } = &events[0]
        else {
            panic!("expected ToolEnd, got {:?}", events[0]);
        };
        assert_eq!(tool_use_id, "tc1");
        assert_eq!(output.as_deref(), Some("file contents"));
        assert!(error.is_none());
    }

    #[test]
    fn tool_result_error_routes_to_error_field() {
        let mut conv = None;
        let events = parse(
            serde_json::json!({"type":"tool","id":"tc2","isError":true,"result":"boom"}),
            &mut conv,
        );
        let ProviderEvent::ToolEnd { output, error, .. } = &events[0] else {
            panic!("expected ToolEnd");
        };
        assert!(output.is_none());
        assert_eq!(error.as_deref(), Some("boom"));
    }

    #[test]
    fn bash_tool_call_passes_through_with_real_result() {
        let mut conv = None;
        let events = parse(
            serde_json::json!({
                "type":"tool_call","toolCallId":"b1","name":"Bash",
                "input":{"command":"ls"}
            }),
            &mut conv,
        );
        // Rendered honestly: the CLI executes its own tools regardless, so
        // no fake denial row.
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ProviderEvent::ToolStart { name, .. } if name == "Bash"));

        // The CLI's real result becomes an ordinary ToolEnd.
        let result = parse(
            serde_json::json!({"type":"tool","toolCallId":"b1","result":"file.txt"}),
            &mut conv,
        );
        assert!(matches!(
            &result[..],
            [ProviderEvent::ToolEnd { tool_use_id, output: Some(o), error: None, .. }]
                if tool_use_id == "b1" && o == "file.txt"
        ));
    }

    #[test]
    fn non_terminal_tool_becomes_tool_start() {
        let mut conv = None;
        let events = parse(
            serde_json::json!({
                "type":"tool_call","toolCallId":"r1","name":"read_file",
                "input":{"path":"a.rs"}
            }),
            &mut conv,
        );
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ProviderEvent::ToolStart { name, .. } if name == "read_file"));
    }
    #[test]
    fn end_event_captures_session_id_and_emits_nothing() {
        let mut conv = None;
        let events = parse(
            serde_json::json!({
                "type":"end","stopReason":"EndTurn","sessionId":"sess-42","requestId":"r1"
            }),
            &mut conv,
        );
        assert!(events.is_empty());
        assert_eq!(conv.as_deref(), Some("sess-42"));
    }

    #[test]
    fn error_reason_extracts_message_only_for_error_type() {
        assert_eq!(
            error_reason(&serde_json::json!({"type":"error","message":"rate limited"})),
            Some("rate limited".to_string())
        );
        // Non-error lines yield None.
        assert_eq!(
            error_reason(&serde_json::json!({"type":"text","data":"hi"})),
            None
        );
        // Error type with no message gets a default.
        assert_eq!(
            error_reason(&serde_json::json!({"type":"error"})),
            Some("grok reported an error".to_string())
        );
    }

    /// A 0.2.x-style terminal frame with no token counts still parses: it
    /// captures the session id and produces no `Usage`.
    #[test]
    fn end_frame_without_usage_yields_no_usage() {
        let mut conv = None;
        let events = parse(
            serde_json::json!({
                "type": "end",
                "stopReason": "EndTurn",
                "sessionId": "abc123",
                "requestId": "xyz789"
            }),
            &mut conv,
        );
        assert!(events.is_empty());
        assert_eq!(conv.as_deref(), Some("abc123"));
    }

    /// The grok 1.0 terminal frame, exactly as the shipped 1.0.3 binary
    /// emits it: per-model counters become one `Usage` per model.
    #[test]
    fn end_frame_with_model_usage_emits_usage() {
        let mut conv = None;
        let events = parse(
            serde_json::json!({
                "type": "end", "stopReason": "end_turn", "sessionId": "s1",
                "requestId": "r1", "num_turns": 1,
                "usage": {
                    "input_tokens": 10, "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 0, "output_tokens": 5,
                    "reasoning_tokens": 2, "total_tokens": 15
                },
                "modelUsage": {
                    "grok-4.5": {
                        "inputTokens": 10, "outputTokens": 5,
                        "cacheReadInputTokens": 3, "cacheCreationInputTokens": 1,
                        "modelCalls": 1
                    }
                }
            }),
            &mut conv,
        );
        assert_eq!(conv.as_deref(), Some("s1"));
        let [
            ProviderEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
                total_tokens,
                context_tokens,
                model,
                ..
            },
        ] = &events[..]
        else {
            panic!("expected one Usage, got {events:?}");
        };
        assert_eq!(*input_tokens, 10);
        assert_eq!(*output_tokens, 5);
        assert_eq!(*cache_read_tokens, 3);
        assert_eq!(*cache_creation_tokens, 1);
        assert_eq!(*context_tokens, 14);
        assert_eq!(*total_tokens, 19);
        assert_eq!(model.as_deref(), Some("grok-4.5"));
    }

    /// A flat `usage` rollup (no `modelUsage`) still produces one `Usage`.
    #[test]
    fn end_frame_falls_back_to_flat_usage() {
        let mut conv = None;
        let events = parse(
            serde_json::json!({
                "type": "end", "stopReason": "end_turn", "sessionId": "s1",
                "usage": {
                    "input_tokens": 10, "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 0, "output_tokens": 5,
                    "total_tokens": 15
                }
            }),
            &mut conv,
        );
        let [
            ProviderEvent::Usage {
                input_tokens,
                output_tokens,
                total_tokens,
                model,
                ..
            },
        ] = &events[..]
        else {
            panic!("expected one Usage, got {events:?}");
        };
        assert_eq!(*input_tokens, 10);
        assert_eq!(*output_tokens, 5);
        assert_eq!(*total_tokens, 15);
        assert!(model.is_none());
    }

    /// The standalone mid-stream `usage` frame duplicates the `end` rollup
    /// and must stay silent, or turns double-count.
    #[test]
    fn standalone_usage_frame_is_ignored() {
        let mut conv = None;
        let events = parse(
            serde_json::json!({
                "type": "usage",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }),
            &mut conv,
        );
        assert!(events.is_empty());
        assert!(events.is_empty());
    }

    /// The grok 1.0 tool-start frame, exactly as documented for the shipped
    /// CLI: `toolName` + `rawInput` instead of `name` + `input`.
    #[test]
    fn acp_tool_call_becomes_tool_start() {
        let mut conv = None;
        let events = parse(
            serde_json::json!({
                "type":"tool_call","toolCallId":"call_1","title":"Read",
                "kind":"read","status":"in_progress","toolName":"read_file",
                "rawInput":{"path":"src/main.rs"},"content":[],"locations":[]
            }),
            &mut conv,
        );
        let [
            ProviderEvent::ToolStart {
                tool_use_id,
                name,
                input,
            },
        ] = &events[..]
        else {
            panic!("expected ToolStart, got {events:?}");
        };
        assert_eq!(tool_use_id, "call_1");
        assert_eq!(name, "read_file");
        assert_eq!(input["path"], "src/main.rs");
    }

    /// A completed `tool_call_update` closes the row with its `rawOutput`.
    #[test]
    fn tool_call_update_completed_becomes_tool_end() {
        let mut conv = None;
        let events = parse(
            serde_json::json!({
                "type":"tool_call_update","toolCallId":"call_1",
                "status":"completed","content":[],"rawOutput":{"lines":42}
            }),
            &mut conv,
        );
        let [
            ProviderEvent::ToolEnd {
                tool_use_id,
                output,
                error,
                ..
            },
        ] = &events[..]
        else {
            panic!("expected ToolEnd, got {events:?}");
        };
        assert_eq!(tool_use_id, "call_1");
        assert_eq!(output.as_deref(), Some("{\"lines\":42}"));
        assert!(error.is_none());
    }

    /// In-flight updates must NOT close the tool row.
    #[test]
    fn tool_call_update_in_progress_is_ignored() {
        let mut conv = None;
        let events = parse(
            serde_json::json!({
                "type":"tool_call_update","toolCallId":"call_1",
                "status":"in_progress","content":[]
            }),
            &mut conv,
        );
        assert!(events.is_empty());
    }

    /// A failed update routes its payload to the error side.
    #[test]
    fn tool_call_update_failed_becomes_tool_end_error() {
        let mut conv = None;
        let events = parse(
            serde_json::json!({
                "type":"tool_call_update","toolCallId":"call_1",
                "status":"failed",
                "content":[{"type":"content","content":{"type":"text","text":"no such file"}}]
            }),
            &mut conv,
        );
        let [ProviderEvent::ToolEnd { output, error, .. }] = &events[..] else {
            panic!("expected ToolEnd, got {events:?}");
        };
        assert!(output.is_none());
        assert_eq!(error.as_deref(), Some("no such file"));
    }

    /// ACP content-block arrays flatten to their text; an empty array is no
    /// output at all (not the string "[]").
    #[test]
    fn tool_call_update_extracts_acp_content_text() {
        let mut conv = None;
        let events = parse(
            serde_json::json!({
                "type":"tool_call_update","toolCallId":"c1","status":"completed",
                "content":[{"type":"content","content":{"type":"text","text":"42 lines"}}]
            }),
            &mut conv,
        );
        let [ProviderEvent::ToolEnd { output, .. }] = &events[..] else {
            panic!("expected ToolEnd");
        };
        assert_eq!(output.as_deref(), Some("42 lines"));

        let events = parse(
            serde_json::json!({
                "type":"tool_call_update","toolCallId":"c2","status":"completed",
                "content":[]
            }),
            &mut conv,
        );
        let [ProviderEvent::ToolEnd { output, .. }] = &events[..] else {
            panic!("expected ToolEnd");
        };
        assert!(output.is_none());
    }

    #[test]
    fn parse_cli_models_oauth_lists_46_then_45() {
        let out = "\
You are logged in with grok.com.

Default model: grok-4.6

Available models:
  * grok-4.6 (default)
  - grok-4.5
";
        let cat = parse_cli_models(out).expect("oauth catalog");
        assert_eq!(cat.default_id.as_deref(), Some("grok-4.6"));
        assert_eq!(cat.models, vec!["grok-4.6", "grok-4.5"]);
    }

    #[test]
    fn parse_cli_models_api_key_lists_only_45() {
        let out = "\
You are using XAI_API_KEY.

Default model: grok-4.5

Available models:
  * grok-4.5 (default)
";
        let cat = parse_cli_models(out).expect("api-key catalog");
        assert_eq!(cat.default_id.as_deref(), Some("grok-4.5"));
        assert_eq!(cat.models, vec!["grok-4.5"]);
    }

    #[test]
    fn parse_cli_models_unauth_lists_only_45() {
        let out = "\
You are not authenticated.

Default model: grok-4.5

Available models:
  * grok-4.5 (default)
";
        let cat = parse_cli_models(out).expect("unauth catalog");
        assert_eq!(cat.models, vec!["grok-4.5"]);
    }

    #[test]
    fn parse_cli_models_rejects_empty_and_garbage() {
        assert_eq!(parse_cli_models(""), None);
        assert_eq!(parse_cli_models("   "), None);
        assert_eq!(parse_cli_models("no models here"), None);
        assert_eq!(
            parse_cli_models("Default model:\n\nAvailable models:\n"),
            None
        );
    }

    #[test]
    fn parse_cli_models_moves_default_to_front() {
        let out = "\
Default model: grok-4.6

Available models:
  - grok-4.5
  - grok-4.6
";
        let cat = parse_cli_models(out).unwrap();
        assert_eq!(cat.models, vec!["grok-4.6", "grok-4.5"]);
    }
}
