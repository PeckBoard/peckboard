//! Stateful Claude CLI `--output-format stream-json` parser.
//!
//! The CLI emits JSON objects in a few shapes (`system`, `assistant`,
//! `content_block_*`, `message_*`, `result`, `user`); each one becomes
//! zero or more `ProviderEvent`s. The parser carries mutable cursors
//! (conversation_id, model_name, current_tool_id, emitted_start) across
//! calls so we can synthesize `Started` and `ToolEnd` events correctly.

use crate::provider::stream::ProviderEvent;

/// Parse a single JSON line from Claude CLI stream-json output into zero or
/// more `ProviderEvent` values.
///
/// The Claude CLI `--output-format stream-json --verbose` emits JSON objects
/// that can take several forms. We handle the common patterns:
///
/// - `{"type":"system","subtype":"init",...}` - initialization with model info
/// - `{"type":"assistant","message":{...}}` - complete message snapshots
/// - `{"type":"content_block_start","content_block":{...}}` - start of a content block
/// - `{"type":"content_block_delta","delta":{...}}` - streamed text chunk
/// - `{"type":"content_block_stop"}` - end of a content block
/// - `{"type":"message_start","message":{...}}` - message start with metadata
/// - `{"type":"message_delta","delta":{...}}` - message-level delta (e.g. stop_reason)
/// - `{"type":"message_stop"}` - message completed
/// - `{"type":"result",...}` - final result with conversation_id
pub(super) fn parse_stream_json(
    json: &serde_json::Value,
    conversation_id: &mut Option<String>,
    model_name: &mut Option<String>,
    current_tool_id: &mut Option<String>,
    emitted_start: &mut bool,
) -> Vec<ProviderEvent> {
    let mut events = Vec::new();

    let msg_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match msg_type {
        // ── system init ──────────────────────────────────────────
        "system" => {
            let subtype = json.get("subtype").and_then(|v| v.as_str()).unwrap_or("");

            if subtype == "init" {
                if let Some(model) = json.get("model").and_then(|v| v.as_str()) {
                    *model_name = Some(model.to_string());
                }
                // CLI uses "session_id" as the resumable conversation identifier
                if let Some(cid) = json
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .or_else(|| json.get("conversation_id").and_then(|v| v.as_str()))
                {
                    *conversation_id = Some(cid.to_string());
                }
                if !*emitted_start {
                    *emitted_start = true;
                    events.push(ProviderEvent::Started {
                        model: model_name.clone().unwrap_or_else(|| "unknown".into()),
                        conversation_id: conversation_id.clone(),
                        metadata: json.clone(),
                    });
                }
            }
        }

        // ── message_start ────────────────────────────────────────
        "message_start" => {
            if let Some(msg) = json.get("message") {
                if let Some(model) = msg.get("model").and_then(|v| v.as_str()) {
                    *model_name = Some(model.to_string());
                }
                if let Some(cid) = msg.get("id").and_then(|v| v.as_str()) {
                    // The message id can serve as a conversation identifier
                    if conversation_id.is_none() {
                        *conversation_id = Some(cid.to_string());
                    }
                }
                if !*emitted_start {
                    *emitted_start = true;
                    events.push(ProviderEvent::Started {
                        model: model_name.clone().unwrap_or_else(|| "unknown".into()),
                        conversation_id: conversation_id.clone(),
                        metadata: json.clone(),
                    });
                }
            }
        }

        // ── content_block_start ──────────────────────────────────
        "content_block_start" => {
            if let Some(block) = json.get("content_block") {
                let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if block_type == "tool_use" {
                    let tool_id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    *current_tool_id = Some(tool_id.clone());
                    events.push(ProviderEvent::ToolStart {
                        tool_use_id: tool_id,
                        name,
                        input: serde_json::Value::Object(serde_json::Map::new()),
                    });
                }
            }
        }

        // ── content_block_delta ──────────────────────────────────
        "content_block_delta" => {
            if let Some(delta) = json.get("delta") {
                let delta_type = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match delta_type {
                    "text_delta" => {
                        if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                            if !text.is_empty() {
                                events.push(ProviderEvent::Text {
                                    text: text.to_string(),
                                });
                            }
                        }
                    }
                    "input_json_delta" => {
                        // Partial JSON for tool input — we emit as text for now
                        // since the full input comes with content_block_stop or
                        // in the assistant message snapshot
                    }
                    _ => {}
                }
            }
        }

        // ── content_block_stop ───────────────────────────────────
        "content_block_stop" => {
            // If we were tracking a tool, emit tool end
            if let Some(tool_id) = current_tool_id.take() {
                events.push(ProviderEvent::ToolEnd {
                    tool_use_id: tool_id,
                    output: None,
                    error: None,
                });
            }
        }

        // ── assistant message snapshot ───────────────────────────
        "assistant" => {
            if let Some(msg) = json.get("message") {
                if let Some(content) = msg.get("content").and_then(|v| v.as_array()) {
                    for block in content {
                        let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        match block_type {
                            "text" => {
                                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                                    if !text.is_empty() {
                                        events.push(ProviderEvent::Text {
                                            text: text.to_string(),
                                        });
                                    }
                                }
                            }
                            "tool_use" => {
                                let tool_id = block
                                    .get("id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let name = block
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown")
                                    .to_string();
                                let input = block
                                    .get("input")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null);
                                events.push(ProviderEvent::ToolStart {
                                    tool_use_id: tool_id,
                                    name,
                                    input,
                                });
                            }
                            "tool_result" => {
                                let tool_id = block
                                    .get("tool_use_id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let output = block
                                    .get("content")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string());
                                let is_error = block
                                    .get("is_error")
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false);
                                let error = if is_error { output.clone() } else { None };
                                events.push(ProviderEvent::ToolEnd {
                                    tool_use_id: tool_id,
                                    output: if is_error { None } else { output },
                                    error,
                                });
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        // ── user message (contains tool results) ─────────────────
        "user" => {
            if let Some(msg) = json.get("message") {
                if let Some(content) = msg.get("content").and_then(|v| v.as_array()) {
                    for block in content {
                        let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        if block_type == "tool_result" {
                            let tool_id = block
                                .get("tool_use_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let output = block
                                .get("content")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            let is_error = block
                                .get("is_error")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            let error = if is_error { output.clone() } else { None };
                            events.push(ProviderEvent::ToolEnd {
                                tool_use_id: tool_id,
                                output: if is_error { None } else { output },
                                error,
                            });
                        }
                    }
                }
            }
        }

        // ── result ───────────────────────────────────────────────
        "result" => {
            // CLI uses "session_id" in result events
            if let Some(cid) = json
                .get("session_id")
                .and_then(|v| v.as_str())
                .or_else(|| json.get("conversation_id").and_then(|v| v.as_str()))
            {
                *conversation_id = Some(cid.to_string());
            }
            // The result event signals completion — we let the process exit
            // handling in stream_events produce the final Completed/Crashed event.
            // But capture the conversation_id for that final event.
        }

        // ── message_delta ────────────────────────────────────────
        "message_delta" => {
            // May contain stop_reason; no action needed for event stream
        }

        // ── message_stop ─────────────────────────────────────────
        "message_stop" => {
            // End of a message turn; completion handled by process exit
        }

        _ => {
            tracing::debug!(msg_type = msg_type, "Unhandled stream-json type");
        }
    }

    events
}

/// Normalize the questions array from an AskUserQuestion control_request input.
///
/// The CLI sends questions as:
/// ```json
/// { "questions": [{ "question": "...", "header": "...", "multiSelect": false,
///     "options": [{ "label": "A", "description": "..." }] }] }
/// ```
///
/// We normalize options to simple label strings for the frontend, preserving
/// the full structure for the control_response answer frame.
pub(super) fn normalize_questions(input: Option<&serde_json::Value>) -> serde_json::Value {
    let empty = serde_json::json!([]);
    let raw_questions = match input
        .and_then(|i| i.get("questions"))
        .and_then(|q| q.as_array())
    {
        Some(q) => q,
        None => return empty,
    };

    let mut result = Vec::new();
    for q in raw_questions {
        let question_text = match q.get("question").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => continue,
        };

        let header = q.get("header").and_then(|v| v.as_str());
        let multi_select = q
            .get("multiSelect")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let mut option_labels = Vec::new();
        let mut option_objects = Vec::new();
        if let Some(options) = q.get("options").and_then(|v| v.as_array()) {
            for opt in options {
                if let Some(label) = opt.get("label").and_then(|v| v.as_str()) {
                    option_labels.push(serde_json::Value::String(label.to_string()));
                    option_objects.push(opt.clone());
                }
            }
        }

        let mut entry = serde_json::json!({
            "question": question_text,
            "multiSelect": multi_select,
            "options": option_labels,
            "optionObjects": option_objects,
        });
        if let Some(h) = header {
            entry["header"] = serde_json::Value::String(h.to_string());
        }
        result.push(entry);
    }

    serde_json::Value::Array(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_system_init() {
        let json = serde_json::json!({
            "type": "system",
            "subtype": "init",
            "model": "claude-sonnet-4-20250514",
            "session_id": "conv-abc123"
        });

        let mut cid = None;
        let mut model = None;
        let mut tool_id = None;
        let mut started = false;

        let events = parse_stream_json(&json, &mut cid, &mut model, &mut tool_id, &mut started);

        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], ProviderEvent::Started { model, .. } if model == "claude-sonnet-4-20250514")
        );
        assert_eq!(cid.as_deref(), Some("conv-abc123"));
        assert!(started);
    }

    #[test]
    fn test_parse_content_block_delta_text() {
        let json = serde_json::json!({
            "type": "content_block_delta",
            "delta": {
                "type": "text_delta",
                "text": "Hello world"
            }
        });

        let mut cid = None;
        let mut model = None;
        let mut tool_id = None;
        let mut started = true;

        let events = parse_stream_json(&json, &mut cid, &mut model, &mut tool_id, &mut started);

        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ProviderEvent::Text { text } if text == "Hello world"));
    }

    #[test]
    fn test_parse_content_block_start_tool_use() {
        let json = serde_json::json!({
            "type": "content_block_start",
            "content_block": {
                "type": "tool_use",
                "id": "tool_123",
                "name": "Read"
            }
        });

        let mut cid = None;
        let mut model = None;
        let mut tool_id = None;
        let mut started = true;

        let events = parse_stream_json(&json, &mut cid, &mut model, &mut tool_id, &mut started);

        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            ProviderEvent::ToolStart { tool_use_id, name, .. }
            if tool_use_id == "tool_123" && name == "Read"
        ));
        assert_eq!(tool_id.as_deref(), Some("tool_123"));
    }

    #[test]
    fn test_parse_content_block_stop_clears_tool() {
        let json = serde_json::json!({ "type": "content_block_stop" });

        let mut cid = None;
        let mut model = None;
        let mut tool_id = Some("tool_123".to_string());
        let mut started = true;

        let events = parse_stream_json(&json, &mut cid, &mut model, &mut tool_id, &mut started);

        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            ProviderEvent::ToolEnd { tool_use_id, .. } if tool_use_id == "tool_123"
        ));
        assert!(tool_id.is_none());
    }

    #[test]
    fn test_parse_result_captures_conversation_id() {
        let json = serde_json::json!({
            "type": "result",
            "session_id": "conv-final-456"
        });

        let mut cid = None;
        let mut model = None;
        let mut tool_id = None;
        let mut started = true;

        let events = parse_stream_json(&json, &mut cid, &mut model, &mut tool_id, &mut started);

        assert!(events.is_empty());
        assert_eq!(cid.as_deref(), Some("conv-final-456"));
    }

    #[test]
    fn test_parse_message_start() {
        let json = serde_json::json!({
            "type": "message_start",
            "message": {
                "id": "msg-abc",
                "model": "claude-sonnet-4-20250514",
                "role": "assistant"
            }
        });

        let mut cid = None;
        let mut model = None;
        let mut tool_id = None;
        let mut started = false;

        let events = parse_stream_json(&json, &mut cid, &mut model, &mut tool_id, &mut started);

        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], ProviderEvent::Started { model, .. } if model == "claude-sonnet-4-20250514")
        );
        assert!(started);
    }

    #[test]
    fn test_parse_ignores_empty_text() {
        let json = serde_json::json!({
            "type": "content_block_delta",
            "delta": {
                "type": "text_delta",
                "text": ""
            }
        });

        let mut cid = None;
        let mut model = None;
        let mut tool_id = None;
        let mut started = true;

        let events = parse_stream_json(&json, &mut cid, &mut model, &mut tool_id, &mut started);
        assert!(events.is_empty());
    }

    #[test]
    fn test_parse_assistant_snapshot() {
        let json = serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [
                    { "type": "text", "text": "Here is the answer." },
                    {
                        "type": "tool_use",
                        "id": "tu_1",
                        "name": "Bash",
                        "input": { "command": "ls" }
                    }
                ]
            }
        });

        let mut cid = None;
        let mut model = None;
        let mut tool_id = None;
        let mut started = true;

        let events = parse_stream_json(&json, &mut cid, &mut model, &mut tool_id, &mut started);

        assert_eq!(events.len(), 2);
        assert!(
            matches!(&events[0], ProviderEvent::Text { text } if text == "Here is the answer.")
        );
        assert!(matches!(
            &events[1],
            ProviderEvent::ToolStart { tool_use_id, name, .. }
            if tool_use_id == "tu_1" && name == "Bash"
        ));
    }

    #[test]
    fn test_no_duplicate_start() {
        let json = serde_json::json!({
            "type": "system",
            "subtype": "init",
            "model": "opus"
        });

        let mut cid = None;
        let mut model = None;
        let mut tool_id = None;
        let mut started = true; // already started

        let events = parse_stream_json(&json, &mut cid, &mut model, &mut tool_id, &mut started);

        // Should not emit another Started event
        assert!(events.is_empty());
        // But should still update model_name
        assert_eq!(model.as_deref(), Some("opus"));
    }
}
