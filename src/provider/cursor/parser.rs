//! Parser for the Cursor CLI (`cursor-agent`) `--output-format stream-json`
//! output, and for its model-discovery output.
//!
//! Cursor's stream-json is designed to be Claude Code-compatible: each line
//! is a JSON object tagged with a `type` of `system` (init), `assistant`
//! (model output — text + `tool_use` blocks), `user` (tool results), or
//! `result` (turn complete). We translate each line into zero or more
//! [`ProviderEvent`]s and carry the `session_id` (Cursor's chat id) out via
//! `conversation_id` so the next turn can `--resume` it.
//!
//! The format isn't formally specified, so every accessor is defensive: an
//! unrecognised shape yields no events rather than an error.

use crate::provider::stream::{ProviderEvent, ToolImage};

/// Parse one JSON line of `cursor-agent` stream-json into provider events.
///
/// `conversation_id` is updated whenever a frame carries a `session_id` /
/// `chat_id` (init and result both do) so the caller can persist it for
/// `--resume`. `emitted_start` guards against emitting more than one
/// `Started` per run when both an `init` frame and the first assistant
/// frame carry model info.
pub(super) fn parse_stream_json(
    json: &serde_json::Value,
    conversation_id: &mut Option<String>,
    model_name: &mut Option<String>,
    emitted_start: &mut bool,
) -> Vec<ProviderEvent> {
    let mut events = Vec::new();
    let msg_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");

    // Both init and result frames can carry the chat id under a few names.
    if let Some(cid) = extract_session_id(json) {
        *conversation_id = Some(cid);
    }

    match msg_type {
        // ── system init ──────────────────────────────────────────
        "system" => {
            if let Some(model) = json.get("model").and_then(|v| v.as_str()) {
                *model_name = Some(model.to_string());
            }
            if !*emitted_start {
                *emitted_start = true;
                events.push(ProviderEvent::Started {
                    model: model_name.clone().unwrap_or_else(|| "cursor".into()),
                    conversation_id: conversation_id.clone(),
                    metadata: json.clone(),
                });
            }
        }

        // ── assistant message (text + tool_use blocks) ───────────
        "assistant" => {
            if let Some(msg) = json.get("message") {
                if let Some(model) = msg.get("model").and_then(|v| v.as_str()) {
                    *model_name = Some(model.to_string());
                }
                // A late init: some runs never emit a `system` frame, so
                // synthesize Started off the first assistant frame.
                if !*emitted_start {
                    *emitted_start = true;
                    events.push(ProviderEvent::Started {
                        model: model_name.clone().unwrap_or_else(|| "cursor".into()),
                        conversation_id: conversation_id.clone(),
                        metadata: json.clone(),
                    });
                }
                push_content_blocks(msg.get("content"), &mut events);
            }
        }

        // ── user message (carries tool results) ──────────────────
        "user" => {
            if let Some(msg) = json.get("message")
                && let Some(blocks) = msg.get("content").and_then(|v| v.as_array())
            {
                for block in blocks {
                    if block.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                        events.push(tool_result_event(block));
                    }
                }
            }
        }

        // ── result (turn complete) ───────────────────────────────
        // The caller emits the final Completed itself (so it can attach the
        // captured conversation_id); we only surface any trailing assistant
        // text the result frame carries, which Cursor sometimes includes.
        "result" => {
            if let Some(text) = json.get("result").and_then(|v| v.as_str())
                && !text.is_empty()
            {
                events.push(ProviderEvent::Text {
                    text: text.to_string(),
                });
            }
        }

        _ => {}
    }

    events
}

/// Pull text + `tool_use` blocks out of an assistant message's `content`,
/// which may be a bare string or an array of typed blocks.
fn push_content_blocks(content: Option<&serde_json::Value>, events: &mut Vec<ProviderEvent>) {
    let Some(content) = content else { return };

    if let Some(text) = content.as_str() {
        if !text.is_empty() {
            events.push(ProviderEvent::Text {
                text: text.to_string(),
            });
        }
        return;
    }

    let Some(blocks) = content.as_array() else {
        return;
    };
    for block in blocks {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(|v| v.as_str())
                    && !text.is_empty()
                {
                    events.push(ProviderEvent::Text {
                        text: text.to_string(),
                    });
                }
            }
            Some("tool_use") => {
                let tool_use_id = block
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("tool")
                    .to_string();
                let input = block
                    .get("input")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                events.push(ProviderEvent::ToolStart {
                    tool_use_id,
                    name,
                    input,
                });
            }
            _ => {}
        }
    }
}

/// Build a `ToolEnd` event from a `tool_result` block, extracting any
/// images the same way the Claude parser does (array-form content with
/// `image` blocks in either the Anthropic `source` envelope or the raw MCP
/// `{mimeType,data}` shape).
fn tool_result_event(block: &serde_json::Value) -> ProviderEvent {
    let tool_use_id = block
        .get("tool_use_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let (output, images) = extract_tool_result(block.get("content"));
    let is_error = block
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let error = if is_error { output.clone() } else { None };
    ProviderEvent::ToolEnd {
        tool_use_id,
        output: if is_error { None } else { output },
        error,
        images,
    }
}

/// Extract textual output and images from a `tool_result` block's `content`
/// (string or array of blocks). Mirrors the Claude provider's helper so
/// screenshots from MCP tools render in the chat the same way.
fn extract_tool_result(content: Option<&serde_json::Value>) -> (Option<String>, Vec<ToolImage>) {
    let Some(content) = content else {
        return (None, Vec::new());
    };
    if let Some(s) = content.as_str() {
        return (Some(s.to_string()), Vec::new());
    }
    let Some(blocks) = content.as_array() else {
        return (None, Vec::new());
    };

    let mut texts: Vec<String> = Vec::new();
    let mut images: Vec<ToolImage> = Vec::new();
    for block in blocks {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|v| v.as_str())
                    && !t.is_empty()
                {
                    texts.push(t.to_string());
                }
            }
            Some("image") => {
                if let Some(img) = parse_image_block(block) {
                    images.push(img);
                }
            }
            _ => {}
        }
    }
    let output = if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n"))
    };
    (output, images)
}

/// Parse an `image` block in either the Anthropic `source.{media_type,data}`
/// envelope or the raw MCP `{mimeType,data}` shape.
fn parse_image_block(block: &serde_json::Value) -> Option<ToolImage> {
    if let Some(source) = block.get("source")
        && let Some(data) = source.get("data").and_then(|v| v.as_str())
    {
        let mime_type = source
            .get("media_type")
            .and_then(|v| v.as_str())
            .unwrap_or("image/png")
            .to_string();
        return Some(ToolImage {
            mime_type,
            data_base64: data.to_string(),
        });
    }
    if let Some(data) = block.get("data").and_then(|v| v.as_str()) {
        let mime_type = block
            .get("mimeType")
            .and_then(|v| v.as_str())
            .unwrap_or("image/png")
            .to_string();
        return Some(ToolImage {
            mime_type,
            data_base64: data.to_string(),
        });
    }
    None
}

/// Pull the chat/session id out of a frame, tolerating the few names
/// Cursor has used (`session_id`, `chatId`, `chat_id`).
fn extract_session_id(json: &serde_json::Value) -> Option<String> {
    for key in ["session_id", "sessionId", "chat_id", "chatId"] {
        if let Some(s) = json.get(key).and_then(|v| v.as_str())
            && !s.is_empty()
        {
            return Some(s.to_string());
        }
    }
    None
}

/// Parse the output of the model-discovery command into a list of model ids.
///
/// JSON only, because the exact discovery output isn't fixed and parsing
/// arbitrary prose as model ids would pollute the picker with garbage from
/// a non-JSON error message. Tolerated shapes:
/// - a JSON array of strings: `["gpt-5", "sonnet-4.5"]`
/// - a JSON array of objects with an `id`/`name`/`model`: `[{"id":"gpt-5"}]`
/// - a JSON object wrapping the array under `models` or `data`
///
/// Returns `None` when the output isn't JSON or carries no ids, so the
/// caller falls back to the static seed.
pub(super) fn parse_cli_models(output: &str) -> Option<Vec<String>> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return None;
    }
    // `cursor-agent models` prints a human-readable table
    // (`<id> - <Display Name>` per line) regardless of `--output-format`,
    // but other/older shapes emit JSON. Try JSON first, then fall back to
    // the line format so discovery picks up the full live model list.
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        let ids = extract_model_ids(&value);
        if !ids.is_empty() {
            return Some(ids);
        }
    }
    let ids = parse_plain_text_models(trimmed);
    if ids.is_empty() { None } else { Some(ids) }
}

/// Parse the CLI's human-readable `models` listing. Each model is a line of
/// the form `<id> - <Display Name>`, where `<id>` is a single whitespace-free
/// token. Header (`Available models`) and footer (`Tip: ...`) lines don't
/// match that shape — their left side contains spaces — so they're skipped.
fn parse_plain_text_models(text: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for line in text.lines() {
        let Some((id, _name)) = line.trim().split_once(" - ") else {
            continue;
        };
        let id = id.trim();
        // A model id is a single token; reject prose that happens to contain
        // " - " (its left side would carry whitespace).
        if id.is_empty() || id.contains(char::is_whitespace) {
            continue;
        }
        if !ids.iter().any(|existing| existing == id) {
            ids.push(id.to_string());
        }
    }
    ids
}

/// Extract model ids from an already-parsed discovery JSON value.
fn extract_model_ids(value: &serde_json::Value) -> Vec<String> {
    let array = if let Some(arr) = value.as_array() {
        Some(arr)
    } else {
        value
            .get("models")
            .or_else(|| value.get("data"))
            .and_then(|v| v.as_array())
    };
    let Some(array) = array else {
        return Vec::new();
    };

    let mut ids = Vec::new();
    for item in array {
        let id = if let Some(s) = item.as_str() {
            Some(s.to_string())
        } else {
            item.get("id")
                .or_else(|| item.get("name"))
                .or_else(|| item.get("model"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        };
        if let Some(id) = id {
            let id = id.trim().to_string();
            if !id.is_empty() {
                ids.push(id);
            }
        }
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(
        json: serde_json::Value,
        conv: &mut Option<String>,
        started: &mut bool,
    ) -> Vec<ProviderEvent> {
        let mut model = None;
        parse_stream_json(&json, conv, &mut model, started)
    }

    #[test]
    fn system_init_emits_started_and_captures_chat_id() {
        let mut conv = None;
        let mut started = false;
        let events = parse(
            serde_json::json!({
                "type": "system",
                "subtype": "init",
                "session_id": "chat-123",
                "model": "gpt-5"
            }),
            &mut conv,
            &mut started,
        );
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            ProviderEvent::Started { model, conversation_id, .. }
            if model == "gpt-5" && conversation_id.as_deref() == Some("chat-123")
        ));
        assert_eq!(conv.as_deref(), Some("chat-123"));
        assert!(started);
    }

    #[test]
    fn assistant_text_and_tool_use() {
        let mut conv = None;
        let mut started = true; // pretend init already happened
        let events = parse(
            serde_json::json!({
                "type": "assistant",
                "message": { "content": [
                    { "type": "text", "text": "Working on it" },
                    { "type": "tool_use", "id": "t1", "name": "Bash",
                      "input": { "command": "ls" } }
                ]}
            }),
            &mut conv,
            &mut started,
        );
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], ProviderEvent::Text { text } if text == "Working on it"));
        assert!(matches!(
            &events[1],
            ProviderEvent::ToolStart { tool_use_id, name, .. }
            if tool_use_id == "t1" && name == "Bash"
        ));
    }

    #[test]
    fn assistant_without_prior_init_synthesizes_started() {
        let mut conv = None;
        let mut started = false;
        let events = parse(
            serde_json::json!({
                "type": "assistant",
                "message": { "model": "auto", "content": [{ "type": "text", "text": "hi" }] }
            }),
            &mut conv,
            &mut started,
        );
        // Started synthesized first, then the text.
        assert!(matches!(&events[0], ProviderEvent::Started { .. }));
        assert!(matches!(&events[1], ProviderEvent::Text { text } if text == "hi"));
        assert!(started);
    }

    #[test]
    fn user_tool_result_with_image() {
        let mut conv = None;
        let mut started = true;
        let events = parse(
            serde_json::json!({
                "type": "user",
                "message": { "content": [{
                    "type": "tool_result",
                    "tool_use_id": "t1",
                    "content": [
                        { "type": "text", "text": "done" },
                        { "type": "image", "source": {
                            "type": "base64", "media_type": "image/png", "data": "QUJD" } }
                    ]
                }]}
            }),
            &mut conv,
            &mut started,
        );
        assert_eq!(events.len(), 1);
        let ProviderEvent::ToolEnd {
            tool_use_id,
            output,
            images,
            ..
        } = &events[0]
        else {
            panic!("expected ToolEnd, got {:?}", events[0]);
        };
        assert_eq!(tool_use_id, "t1");
        assert_eq!(output.as_deref(), Some("done"));
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime_type, "image/png");
        assert_eq!(images[0].data_base64, "QUJD");
    }

    #[test]
    fn result_captures_chat_id_and_trailing_text() {
        let mut conv = None;
        let mut started = true;
        let events = parse(
            serde_json::json!({
                "type": "result",
                "subtype": "success",
                "chatId": "chat-9",
                "result": "All set."
            }),
            &mut conv,
            &mut started,
        );
        assert_eq!(conv.as_deref(), Some("chat-9"));
        assert!(matches!(&events[0], ProviderEvent::Text { text } if text == "All set."));
    }

    #[test]
    fn parse_cli_models_json_array_of_strings() {
        assert_eq!(
            parse_cli_models(r#"["gpt-5", "sonnet-4.5", "  ", "opus-4.1"]"#),
            Some(vec!["gpt-5".into(), "sonnet-4.5".into(), "opus-4.1".into()])
        );
    }

    #[test]
    fn parse_cli_models_json_objects_and_wrappers() {
        assert_eq!(
            parse_cli_models(r#"[{"id":"gpt-5"},{"name":"opus-4.1"}]"#),
            Some(vec!["gpt-5".into(), "opus-4.1".into()])
        );
        assert_eq!(
            parse_cli_models(r#"{"models":[{"id":"auto"},{"id":"gpt-5"}]}"#),
            Some(vec!["auto".into(), "gpt-5".into()])
        );
        assert_eq!(
            parse_cli_models(r#"{"data":["sonnet-4.5"]}"#),
            Some(vec!["sonnet-4.5".into()])
        );
    }

    #[test]
    fn parse_cli_models_rejects_non_json_and_empty() {
        // Empty → None so caller seeds statically.
        assert_eq!(parse_cli_models("   "), None);
        // Valid JSON object with nothing usable → None.
        assert_eq!(parse_cli_models("{}"), None);
        // Prose with no `<id> - <name>` lines → None.
        assert_eq!(parse_cli_models("no models are available here"), None);
    }

    #[test]
    fn parse_cli_models_plain_text_table() {
        // The shape `cursor-agent models` actually emits: a header, one
        // `<id> - <Display Name>` line per model (some with trailing markers),
        // then a tip footer. Header and footer must be skipped.
        let out = "Available models\n\
            \n\
            auto - Auto\n\
            gpt-5.3-codex - Codex 5.3\n\
            composer-2.5 - Composer 2.5 (current)\n\
            composer-2.5-fast - Composer 2.5 Fast (default)\n\
            claude-opus-4-8-thinking-high - Opus 4.8 1M Thinking\n\
            claude-fable-5-low - Fable 5 1M Low (NO ZDR)\n\
            \n\
            Tip: use --model <id> (or /model <id> in interactive mode) to switch.";
        assert_eq!(
            parse_cli_models(out),
            Some(vec![
                "auto".into(),
                "gpt-5.3-codex".into(),
                "composer-2.5".into(),
                "composer-2.5-fast".into(),
                "claude-opus-4-8-thinking-high".into(),
                "claude-fable-5-low".into(),
            ])
        );
    }
}
