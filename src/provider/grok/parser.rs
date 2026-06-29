//! Parser for `grok -p --output-format streaming-json` output.
//!
//! Grok's headless streaming-json is newline-delimited, one flat JSON object
//! per line tagged with a `type`:
//!
//! ```json
//! {"type":"text","data":"Here's"}
//! {"type":"thought","data":"Analyzing the directory structure..."}
//! {"type":"tool_call","name":"read_file","input":{"path":"x"}}
//! {"type":"tool","toolCallId":"...","result":"..."}
//! {"type":"end","stopReason":"EndTurn","sessionId":"abc123","requestId":"xyz"}
//! ```
//!
//! We translate each line into zero or more [`ProviderEvent`]s and carry the
//! `sessionId` out via `conversation_id` so the next turn can resume it with
//! `--session-id`. The `Started` and final `Completed` events are emitted by
//! the run loop (which owns the model label and the captured id), so this
//! parser never produces them. `thought` (reasoning) has no channel in the
//! unified event model — matching the Claude/Cursor providers, which also
//! don't surface reasoning as assistant text — so it is dropped.
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

        // Reasoning / chain-of-thought — no unified channel, dropped.
        "thought" => {}

        // A tool invocation begins.
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
                .get("input")
                .or_else(|| json.get("args"))
                .or_else(|| json.get("arguments"))
                .or_else(|| json.get("parameters"))
                .or_else(|| json.get("params"))
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            events.push(ProviderEvent::ToolStart {
                tool_use_id,
                name,
                input,
            });
        }

        // A tool finished, carrying its result.
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

        // `end` carries the session id (captured above); the run loop emits
        // the terminal Completed itself. `error` is inspected by the run loop
        // for a crash reason, so it produces no event here either.
        _ => {}
    }

    events
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
/// non-string payload (serialised to JSON).
fn tool_output(json: &serde_json::Value) -> Option<String> {
    let value = json
        .get("result")
        .or_else(|| json.get("output"))
        .or_else(|| json.get("content"))
        .or_else(|| json.get("data"))?;
    match value {
        serde_json::Value::String(s) if s.is_empty() => None,
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Null => None,
        other => Some(other.to_string()),
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
    fn thought_is_dropped() {
        let mut conv = None;
        let events = parse(
            serde_json::json!({"type":"thought","data":"hmm let me think"}),
            &mut conv,
        );
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
}
