//! Parser for `kimi --prompt --output-format stream-json` output.
//!
//! Kimi Code's prompt-mode stream-json is newline-delimited, one JSON object
//! per line, shaped like OpenAI chat messages plus `meta` frames (verified
//! against the CLI's `PromptJsonWriter`, kimi-code 0.27.0):
//!
//! ```json
//! {"role":"meta","type":"system.version","version":"0.27.0"}
//! {"role":"assistant","content":"Let me look.","tool_calls":[{"type":"function","id":"tc_1","function":{"name":"ReadFile","arguments":"{\"path\":\"x\"}"}}]}
//! {"role":"tool","tool_call_id":"tc_1","content":"..."}
//! {"role":"meta","type":"turn.step.retrying","failed_attempt":1}
//! {"type":"goal.summary","goalId":null,"status":null}
//! {"role":"meta","type":"session.resume_hint","session_id":"abc","command":"kimi -r abc"}
//! ```
//!
//! Assistant text is flushed in blocks (before every tool result and at turn
//! end), not streamed as deltas; thinking is never written. The session id
//! rides the trailing `session.resume_hint` meta frame and is carried out via
//! `conversation_id` so the next turn can resume with `--session`. `Started`
//! and the final `Completed` are emitted by the run loop, never here.
//!
//! The shape isn't formally specified, so every accessor is defensive: an
//! unrecognised line yields no events rather than an error.

use crate::provider::stream::ProviderEvent;

/// Parse one JSON line of kimi stream-json into provider events, updating
/// `conversation_id` from any `session_id` the line carries (the trailing
/// `session.resume_hint` frame always does).
pub(super) fn parse_stream_json(
    json: &serde_json::Value,
    conversation_id: &mut Option<String>,
    denied_tool_ids: &mut std::collections::HashSet<String>,
) -> Vec<ProviderEvent> {
    let mut events = Vec::new();

    if let Some(cid) = extract_session_id(json) {
        *conversation_id = Some(cid);
    }

    match json.get("role").and_then(|v| v.as_str()).unwrap_or("") {
        "assistant" => {
            if let Some(text) = json.get("content").and_then(|v| v.as_str())
                && !text.is_empty()
            {
                events.push(ProviderEvent::Text {
                    text: text.to_string(),
                });
            }
            for call in json
                .get("tool_calls")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                let tool_use_id = tool_id(call);
                let function = call.get("function");
                let name = function
                    .and_then(|f| f.get("name"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("tool")
                    .to_string();
                let input = tool_arguments(function);
                if crate::provider::is_terminal_tool(&name) {
                    // Kimi's prompt mode auto-approves tools with no
                    // pre-execution gate, so peckboard cannot stop the CLI
                    // from executing a terminal command. Surface it as a
                    // denied tool call and drop the real result (see the
                    // `tool` arm); the WORKING_STYLE prompt stays the only
                    // model-side deterrent — matching the grok provider.
                    denied_tool_ids.insert(tool_use_id.clone());
                    events.push(ProviderEvent::ToolStart {
                        tool_use_id: tool_use_id.clone(),
                        name,
                        input,
                    });
                    events.push(ProviderEvent::ToolEnd {
                        tool_use_id,
                        output: None,
                        error: Some(crate::provider::TERMINAL_TOOL_DISABLED_MSG.to_string()),
                        images: Vec::new(),
                    });
                } else {
                    events.push(ProviderEvent::ToolStart {
                        tool_use_id,
                        name,
                        input,
                    });
                }
            }
        }

        // A tool finished, carrying its stringified output. The writer has no
        // error channel — failures arrive as ordinary content text.
        "tool" => {
            let tool_use_id = tool_id(json);
            if denied_tool_ids.remove(&tool_use_id) {
                // Result of a terminal tool we already denied at the call;
                // drop the CLI's real output so it never enters the
                // transcript.
                return events;
            }
            let output = json
                .get("content")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .filter(|s| !s.is_empty());
            events.push(ProviderEvent::ToolEnd {
                tool_use_id,
                output,
                error: None,
                images: Vec::new(),
            });
        }

        // `meta` frames (system.version, turn.step.retrying, and
        // session.resume_hint — whose session id is captured above) and the
        // role-less goal.summary line carry no transcript content.
        _ => {}
    }

    events
}

/// A tool call/result id under either of the names kimi uses. Falls back to
/// the function name, then a constant, so a `ToolStart` and its `ToolEnd`
/// still pair up even when the id field is absent.
fn tool_id(json: &serde_json::Value) -> String {
    for key in ["tool_call_id", "id"] {
        if let Some(s) = json.get(key).and_then(|v| v.as_str())
            && !s.is_empty()
        {
            return s.to_string();
        }
    }
    json.get("function")
        .and_then(|f| f.get("name"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("tool")
        .to_string()
}

/// A tool call's input: kimi serialises `function.arguments` as a JSON
/// *string* (OpenAI style). Parse it back to a value; keep the raw string if
/// it isn't valid JSON (e.g. truncated arguments).
fn tool_arguments(function: Option<&serde_json::Value>) -> serde_json::Value {
    let Some(raw) = function
        .and_then(|f| f.get("arguments"))
        .and_then(|v| v.as_str())
    else {
        return serde_json::Value::Null;
    };
    if raw.is_empty() {
        return serde_json::Value::Null;
    }
    serde_json::from_str(raw).unwrap_or(serde_json::Value::String(raw.to_string()))
}

/// Pull the session id out of a `session.resume_hint` meta frame (tolerating
/// a camelCase variant).
fn extract_session_id(json: &serde_json::Value) -> Option<String> {
    for key in ["session_id", "sessionId"] {
        if let Some(s) = json.get(key).and_then(|v| v.as_str())
            && !s.is_empty()
        {
            return Some(s.to_string());
        }
    }
    None
}

/// Parse `kimi provider list --json` output into the configured model
/// aliases (the strings `--model` accepts). `None` when the output isn't the
/// expected shape.
pub(super) fn parse_cli_models(text: &str) -> Option<Vec<String>> {
    let json: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    let models = json.get("models")?.as_object()?;
    let mut ids: Vec<String> = models.keys().cloned().collect();
    ids.sort();
    Some(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: serde_json::Value, conv: &mut Option<String>) -> Vec<ProviderEvent> {
        let mut denied = std::collections::HashSet::new();
        parse_stream_json(&json, conv, &mut denied)
    }

    #[test]
    fn assistant_content_becomes_text() {
        let mut conv = None;
        let events = parse(
            serde_json::json!({"role": "assistant", "content": "Hello"}),
            &mut conv,
        );
        assert!(matches!(&events[..], [ProviderEvent::Text { text }] if text == "Hello"));
    }

    #[test]
    fn empty_or_absent_content_is_dropped() {
        let mut conv = None;
        assert!(
            parse(
                serde_json::json!({"role": "assistant", "content": ""}),
                &mut conv
            )
            .is_empty()
        );
        assert!(parse(serde_json::json!({"role": "assistant"}), &mut conv).is_empty());
    }

    #[test]
    fn tool_calls_become_tool_start_with_parsed_arguments() {
        let mut conv = None;
        let events = parse(
            serde_json::json!({
                "role": "assistant",
                "content": "Reading.",
                "tool_calls": [{
                    "type": "function",
                    "id": "tc_1",
                    "function": {"name": "ReadFile", "arguments": "{\"path\":\"src/main.rs\"}"}
                }]
            }),
            &mut conv,
        );
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], ProviderEvent::Text { text } if text == "Reading."));
        match &events[1] {
            ProviderEvent::ToolStart {
                tool_use_id,
                name,
                input,
            } => {
                assert_eq!(tool_use_id, "tc_1");
                assert_eq!(name, "ReadFile");
                assert_eq!(input["path"], "src/main.rs");
            }
            other => panic!("expected ToolStart, got {other:?}"),
        }
    }

    #[test]
    fn invalid_arguments_json_is_kept_as_raw_string() {
        let mut conv = None;
        let events = parse(
            serde_json::json!({
                "role": "assistant",
                "tool_calls": [{
                    "type": "function",
                    "id": "tc_1",
                    "function": {"name": "ReadFile", "arguments": "{\"pa"}
                }]
            }),
            &mut conv,
        );
        match &events[..] {
            [ProviderEvent::ToolStart { input, .. }] => {
                assert_eq!(input, &serde_json::Value::String("{\"pa".into()));
            }
            other => panic!("expected one ToolStart, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_becomes_tool_end() {
        let mut conv = None;
        let events = parse(
            serde_json::json!({"role": "tool", "tool_call_id": "tc_1", "content": "42 lines"}),
            &mut conv,
        );
        match &events[..] {
            [
                ProviderEvent::ToolEnd {
                    tool_use_id,
                    output,
                    error,
                    ..
                },
            ] => {
                assert_eq!(tool_use_id, "tc_1");
                assert_eq!(output.as_deref(), Some("42 lines"));
                assert!(error.is_none());
            }
            other => panic!("expected one ToolEnd, got {other:?}"),
        }
    }

    #[test]
    fn empty_tool_content_maps_to_no_output() {
        let mut conv = None;
        let events = parse(
            serde_json::json!({"role": "tool", "tool_call_id": "tc_1", "content": ""}),
            &mut conv,
        );
        assert!(matches!(&events[..], [ProviderEvent::ToolEnd { output, .. }] if output.is_none()));
    }

    #[test]
    fn terminal_tool_is_denied_and_real_result_suppressed() {
        let mut conv = None;
        let mut denied = std::collections::HashSet::new();
        let start = parse_stream_json(
            &serde_json::json!({
                "role": "assistant",
                "tool_calls": [{
                    "type": "function",
                    "id": "tc_9",
                    "function": {"name": "Bash", "arguments": "{\"command\":\"rm -rf /\"}"}
                }]
            }),
            &mut conv,
            &mut denied,
        );
        assert_eq!(start.len(), 2);
        assert!(matches!(&start[0], ProviderEvent::ToolStart { name, .. } if name == "Bash"));
        assert!(matches!(
            &start[1],
            ProviderEvent::ToolEnd { error: Some(e), .. }
                if e == crate::provider::TERMINAL_TOOL_DISABLED_MSG
        ));

        // The CLI's real result line for that id is dropped.
        let result = parse_stream_json(
            &serde_json::json!({"role": "tool", "tool_call_id": "tc_9", "content": "done"}),
            &mut conv,
            &mut denied,
        );
        assert!(result.is_empty());
        assert!(denied.is_empty());
    }

    #[test]
    fn resume_hint_captures_session_id_and_emits_nothing() {
        let mut conv = None;
        let events = parse(
            serde_json::json!({
                "role": "meta",
                "type": "session.resume_hint",
                "session_id": "abc123",
                "command": "kimi -r abc123"
            }),
            &mut conv,
        );
        assert!(events.is_empty());
        assert_eq!(conv.as_deref(), Some("abc123"));
    }

    #[test]
    fn version_retrying_and_goal_summary_are_ignored() {
        let mut conv = None;
        for line in [
            serde_json::json!({"role": "meta", "type": "system.version", "version": "0.27.0"}),
            serde_json::json!({"role": "meta", "type": "turn.step.retrying", "failed_attempt": 1}),
            serde_json::json!({"type": "goal.summary", "goalId": null, "status": null}),
        ] {
            assert!(parse(line, &mut conv).is_empty());
        }
        assert!(conv.is_none());
    }

    #[test]
    fn parse_cli_models_reads_alias_keys() {
        let ids = parse_cli_models(
            r#"{"providers":{"moonshot":{"type":"kimi"}},"models":{"kimi-for-coding":{"provider":"moonshot"},"k2-thinking":{"provider":"moonshot"}}}"#,
        )
        .unwrap();
        assert_eq!(ids, vec!["k2-thinking", "kimi-for-coding"]);
    }

    #[test]
    fn parse_cli_models_rejects_garbage() {
        assert!(parse_cli_models("not json").is_none());
        assert!(parse_cli_models(r#"{"providers":{}}"#).is_none());
    }
}
