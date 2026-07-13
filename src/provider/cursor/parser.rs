//! Parser for the Cursor CLI (`cursor-agent`) `--output-format stream-json`
//! output, and for its model-discovery output.
//!
//! Cursor's stream-json is newline-delimited, one JSON object per line
//! tagged with a `type` (shapes verified against cursor-agent 2026.07.09):
//!
//! - `system` (subtype `init`) — carries the chat `session_id` and model.
//! - `assistant` — model text. With `--stream-partial-output` each text
//!   segment arrives as a series of small delta frames followed by ONE
//!   cumulative snapshot frame repeating the whole segment; the parser
//!   accumulates deltas per segment and swallows a frame that exactly
//!   equals the accumulation. Claude-style `tool_use` blocks are also
//!   handled defensively, though current CLIs emit `tool_call` frames.
//! - `tool_call` (subtype `started` / `completed`) — one frame per tool
//!   action. The payload sits under the single `tool_call` key ending in
//!   `ToolCall` (`readToolCall`, `globToolCall`, `shellToolCall`, …) with
//!   `args` and, on completion, `result.success` / `result.error`. Mapped
//!   to `ToolStart` / `ToolEnd` so the UI shows Cursor's actions live,
//!   matching the Claude provider's verbosity.
//! - `user` — Claude-style `tool_result` blocks (compatibility path; the
//!   initial prompt echo carries only `text` blocks and yields nothing).
//! - `result` — turn complete. Carries the chat id and token `usage`
//!   (emitted as a `Usage` event); its `result` string repeats the turn's
//!   full assistant text, so it is surfaced only when no assistant text
//!   was streamed.
//!
//! The format isn't formally specified, so every accessor is defensive: an
//! unrecognised shape yields no events rather than an error.

use crate::provider::stream::{ProviderEvent, ToolImage};

/// Cap on tool output carried on a `ToolEnd`. Cursor `completed` frames
/// embed entire tool payloads (a `readToolCall` result holds the whole
/// file), so an uncapped copy would balloon the event log and every WS
/// broadcast.
const MAX_TOOL_OUTPUT_BYTES: usize = 64 * 1024;

/// Mutable per-turn parser state, owned by the run loop and threaded
/// through [`parse_stream_json`] one stdout line at a time.
#[derive(Default)]
pub(super) struct TurnState {
    /// Cursor chat id, captured from any frame carrying one; the run loop
    /// attaches it to the final `Completed` so the next turn can `--resume`.
    pub conversation_id: Option<String>,
    /// Model name reported by the init/assistant frames.
    pub model_name: Option<String>,
    /// Guards against emitting more than one `Started` per run.
    pub emitted_start: bool,
    /// Terminal-tool calls denied at their start; the matching `completed`
    /// frame (or Claude-style `tool_result`) is dropped so the real output
    /// never lands in the transcript.
    denied_tool_ids: std::collections::HashSet<String>,
    /// Delta text accumulated for the current segment; an assistant frame
    /// exactly equal to it is the segment's cumulative snapshot → swallowed.
    text_acc: String,
    /// Whether any assistant text was emitted this turn; suppresses the
    /// `result` frame's full-turn text echo.
    saw_text: bool,
}

/// Parse one JSON line of `cursor-agent` stream-json into provider events,
/// updating `state` as frames reveal the chat id, model, and tool lifecycle.
pub(super) fn parse_stream_json(
    json: &serde_json::Value,
    state: &mut TurnState,
) -> Vec<ProviderEvent> {
    let mut events = Vec::new();
    let msg_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");

    // Most frame types can carry the chat id under a few names.
    if let Some(cid) = extract_session_id(json) {
        state.conversation_id = Some(cid);
    }
    // Deltas and their snapshot are always contiguous assistant frames, so
    // any other frame type closes the current text segment.
    if msg_type != "assistant" {
        state.text_acc.clear();
    }

    match msg_type {
        // ── system init ──────────────────────────────────────────
        "system" => {
            if let Some(model) = json.get("model").and_then(|v| v.as_str()) {
                state.model_name = Some(model.to_string());
            }
            synth_started(state, &mut events, json.clone());
        }

        // ── assistant message (text; legacy tool_use blocks) ─────
        "assistant" => {
            if let Some(msg) = json.get("message") {
                if let Some(model) = msg.get("model").and_then(|v| v.as_str()) {
                    state.model_name = Some(model.to_string());
                }
                // Some runs never emit a `system` frame, so synthesize
                // Started off the first assistant frame.
                synth_started(state, &mut events, json.clone());
                push_content_blocks(msg.get("content"), &mut events, state);
            }
        }

        // ── tool_call lifecycle (started / completed) ────────────
        "tool_call" => {
            synth_started(
                state,
                &mut events,
                serde_json::json!({ "provider": "cursor" }),
            );
            push_tool_call(json, &mut events, state);
        }

        // ── user message (Claude-style tool results) ─────────────
        "user" => {
            if let Some(msg) = json.get("message")
                && let Some(blocks) = msg.get("content").and_then(|v| v.as_array())
            {
                for block in blocks {
                    if block.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                        let id = block
                            .get("tool_use_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if state.denied_tool_ids.remove(id) {
                            // Real result of a terminal tool we denied at its
                            // `tool_use`; drop it so its output never lands.
                            continue;
                        }
                        events.push(tool_result_event(block));
                    }
                }
            }
        }

        // ── result (turn complete) ───────────────────────────────
        // The caller emits the final Completed itself (so it can attach the
        // captured conversation_id). The frame's `result` string repeats the
        // turn's full assistant text, so it's only surfaced when nothing was
        // streamed; the `usage` rollup becomes a Usage event.
        "result" => {
            if !state.saw_text
                && let Some(text) = json.get("result").and_then(|v| v.as_str())
                && !text.is_empty()
            {
                events.push(ProviderEvent::Text {
                    text: text.to_string(),
                });
            }
            if let Some(usage) = usage_event(json, state) {
                events.push(usage);
            }
        }

        _ => {}
    }

    events
}

/// Emit `Started` once per run, with whatever model/chat info is known.
fn synth_started(
    state: &mut TurnState,
    events: &mut Vec<ProviderEvent>,
    metadata: serde_json::Value,
) {
    if state.emitted_start {
        return;
    }
    state.emitted_start = true;
    events.push(ProviderEvent::Started {
        model: state.model_name.clone().unwrap_or_else(|| "cursor".into()),
        conversation_id: state.conversation_id.clone(),
        metadata,
    });
}

fn push_content_blocks(
    content: Option<&serde_json::Value>,
    events: &mut Vec<ProviderEvent>,
    state: &mut TurnState,
) {
    let Some(content) = content else { return };

    if let Some(text) = content.as_str() {
        push_text(text, events, state);
        return;
    }

    let Some(blocks) = content.as_array() else {
        return;
    };
    for block in blocks {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                    push_text(text, events, state);
                }
            }
            Some("tool_use") => {
                // Legacy Claude-compatible shape; current CLIs emit
                // dedicated `tool_call` frames instead.
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
                push_tool_start(tool_use_id, name, input, events, state);
            }
            _ => {}
        }
    }
}

/// Emit a Text event, deduplicating `--stream-partial-output`'s cumulative
/// segment snapshots: deltas are accumulated, and a frame whose text equals
/// the accumulation is the snapshot of what was already emitted → swallowed
/// (and the segment reset). Without the flag the accumulator is empty when
/// each full-segment frame arrives, so they pass straight through.
fn push_text(text: &str, events: &mut Vec<ProviderEvent>, state: &mut TurnState) {
    if text.is_empty() {
        return;
    }
    if !state.text_acc.is_empty() && text == state.text_acc {
        state.text_acc.clear();
        return;
    }
    state.text_acc.push_str(text);
    state.saw_text = true;
    events.push(ProviderEvent::Text {
        text: text.to_string(),
    });
}

/// Emit `ToolStart` — and, for a terminal tool, an immediate denied
/// `ToolEnd`. cursor-agent's headless `--force` runs tools autonomously
/// with no pre-execution gate, so peckboard cannot stop the CLI from
/// executing a terminal command. Surface it as a denied tool call and drop
/// the real result when it arrives; the model's own context is unchanged,
/// so the WORKING_STYLE prompt stays the only model-side deterrent.
fn push_tool_start(
    tool_use_id: String,
    name: String,
    input: serde_json::Value,
    events: &mut Vec<ProviderEvent>,
    state: &mut TurnState,
) {
    if is_denied_tool(&name) {
        state.denied_tool_ids.insert(tool_use_id.clone());
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

/// Cursor's shell tool (`shellToolCall` → "shell") plus the shared
/// Claude-CLI terminal names, so command execution stays behind the
/// approval-gated `run_command` MCP tool.
fn is_denied_tool(name: &str) -> bool {
    name == "shell" || crate::provider::is_terminal_tool(name)
}

/// Translate a `tool_call` frame into `ToolStart` / `ToolEnd`.
///
/// The tool payload sits under the single key of `tool_call` ending in
/// `ToolCall`; the prefix is the tool name (`readToolCall` → `read`). The
/// frame-level `call_id` pairs `started` with `completed`.
fn push_tool_call(
    json: &serde_json::Value,
    events: &mut Vec<ProviderEvent>,
    state: &mut TurnState,
) {
    let Some(tc) = json.get("tool_call").and_then(|v| v.as_object()) else {
        return;
    };
    let payload = tc
        .iter()
        .find(|(k, v)| k.ends_with("ToolCall") && v.is_object());
    let name = payload
        .map(|(k, _)| k.strip_suffix("ToolCall").unwrap_or(k))
        .filter(|n| !n.is_empty())
        .unwrap_or("tool")
        .to_string();
    let tool_use_id = json
        .get("call_id")
        .and_then(|v| v.as_str())
        .or_else(|| tc.get("toolCallId").and_then(|v| v.as_str()))
        .map(str::to_string)
        .unwrap_or_else(|| name.clone());
    let payload = payload.map(|(_, v)| v);

    match json.get("subtype").and_then(|v| v.as_str()).unwrap_or("") {
        "started" => {
            let input = payload
                .and_then(|p| p.get("args"))
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            push_tool_start(tool_use_id, name, input, events, state);
        }
        "completed" => {
            if state.denied_tool_ids.remove(&tool_use_id) {
                // Real result of a terminal tool denied at `started`; drop
                // it so its output never lands.
                return;
            }
            let (output, error) = tool_call_outcome(payload.and_then(|p| p.get("result")));
            events.push(ProviderEvent::ToolEnd {
                tool_use_id,
                output,
                error,
                images: Vec::new(),
            });
        }
        _ => {}
    }
}

/// Split a `tool_call.result` into `(output, error)` for `ToolEnd`.
/// `error.errorMessage` wins; `success` prefers its `content` string (file
/// reads) over a pretty-JSON dump of the whole payload. Everything is
/// capped at [`MAX_TOOL_OUTPUT_BYTES`].
fn tool_call_outcome(result: Option<&serde_json::Value>) -> (Option<String>, Option<String>) {
    let Some(result) = result else {
        return (None, None);
    };
    if let Some(err) = result.get("error") {
        let msg = err
            .get("errorMessage")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| err.as_str().map(str::to_string))
            .unwrap_or_else(|| err.to_string());
        return (None, Some(cap_output(msg)));
    }
    if let Some(success) = result.get("success") {
        let text = success
            .get("content")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| success.as_str().map(str::to_string))
            .unwrap_or_else(|| serde_json::to_string_pretty(success).unwrap_or_default());
        let output = if text.is_empty() {
            None
        } else {
            Some(cap_output(text))
        };
        return (output, None);
    }
    // Unknown result shape — carry it verbatim so nothing is silently lost.
    match result {
        serde_json::Value::Null => (None, None),
        other => (Some(cap_output(other.to_string())), None),
    }
}

/// Cap `s` at [`MAX_TOOL_OUTPUT_BYTES`], appending a truncation marker.
fn cap_output(s: String) -> String {
    if s.len() <= MAX_TOOL_OUTPUT_BYTES {
        return s;
    }
    let mut end = MAX_TOOL_OUTPUT_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… [truncated {} bytes]", &s[..end], s.len() - end)
}

/// Build a `Usage` event from a result frame's `usage` rollup
/// (`inputTokens` / `outputTokens` / `cacheReadTokens` /
/// `cacheWriteTokens`). `None` when the frame carries no usage or all
/// counters are zero.
fn usage_event(json: &serde_json::Value, state: &TurnState) -> Option<ProviderEvent> {
    let usage = json.get("usage")?;
    let count = |key: &str| usage.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
    let input = count("inputTokens");
    let output = count("outputTokens");
    let cache_read = count("cacheReadTokens");
    let cache_write = count("cacheWriteTokens");
    if input + output + cache_read + cache_write == 0 {
        return None;
    }
    // Same convention as the Claude provider: context is the window at end
    // of turn, total adds the generated output on top.
    let context = input + cache_read + cache_write;
    Some(ProviderEvent::Usage {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_creation_tokens: cache_write,
        total_tokens: context + output,
        context_tokens: context,
        model: state.model_name.clone(),
        turn_seq: None,
    })
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

    fn parse(json: serde_json::Value, state: &mut TurnState) -> Vec<ProviderEvent> {
        parse_stream_json(&json, state)
    }

    /// State as if `system` init already emitted `Started`.
    fn started_state() -> TurnState {
        TurnState {
            emitted_start: true,
            ..Default::default()
        }
    }

    #[test]
    fn legacy_bash_tool_use_is_denied_and_real_result_suppressed() {
        let mut state = started_state();
        let events = parse(
            serde_json::json!({
                "type": "assistant",
                "message": { "content": [
                    { "type": "tool_use", "id": "t1", "name": "Bash",
                      "input": { "command": "ls" } }
                ]}
            }),
            &mut state,
        );
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], ProviderEvent::ToolStart { name, .. } if name == "Bash"));
        let ProviderEvent::ToolEnd { output, error, .. } = &events[1] else {
            panic!("expected ToolEnd, got {:?}", events[1]);
        };
        assert!(output.is_none());
        assert_eq!(
            error.as_deref(),
            Some(crate::provider::TERMINAL_TOOL_DISABLED_MSG)
        );
        assert!(state.denied_tool_ids.contains("t1"));

        // The matching tool_result frame is dropped entirely.
        let result = parse(
            serde_json::json!({
                "type": "user",
                "message": { "content": [
                    { "type": "tool_result", "tool_use_id": "t1", "content": "ran" }
                ]}
            }),
            &mut state,
        );
        assert!(result.is_empty());
        assert!(!state.denied_tool_ids.contains("t1"));
    }

    #[test]
    fn non_terminal_tool_result_still_emitted() {
        let mut state = started_state();
        let events = parse(
            serde_json::json!({
                "type": "user",
                "message": { "content": [
                    { "type": "tool_result", "tool_use_id": "r1", "content": "ok" }
                ]}
            }),
            &mut state,
        );
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ProviderEvent::ToolEnd { .. }));
    }

    #[test]
    fn system_init_emits_started_and_captures_chat_id() {
        let mut state = TurnState::default();
        let events = parse(
            serde_json::json!({
                "type": "system",
                "subtype": "init",
                "session_id": "chat-123",
                "model": "gpt-5"
            }),
            &mut state,
        );
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            ProviderEvent::Started { model, conversation_id, .. }
            if model == "gpt-5" && conversation_id.as_deref() == Some("chat-123")
        ));
        assert_eq!(state.conversation_id.as_deref(), Some("chat-123"));
        assert!(state.emitted_start);
    }

    #[test]
    fn assistant_text_and_tool_use() {
        let mut state = started_state();
        let events = parse(
            serde_json::json!({
                "type": "assistant",
                "message": { "content": [
                    { "type": "text", "text": "Working on it" },
                    { "type": "tool_use", "id": "t1", "name": "read_file",
                      "input": { "path": "a.rs" } }
                ]}
            }),
            &mut state,
        );
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], ProviderEvent::Text { text } if text == "Working on it"));
        assert!(matches!(
            &events[1],
            ProviderEvent::ToolStart { tool_use_id, name, .. }
            if tool_use_id == "t1" && name == "read_file"
        ));
    }

    #[test]
    fn assistant_without_prior_init_synthesizes_started() {
        let mut state = TurnState::default();
        let events = parse(
            serde_json::json!({
                "type": "assistant",
                "message": { "model": "auto", "content": [{ "type": "text", "text": "hi" }] }
            }),
            &mut state,
        );
        // Started synthesized first, then the text.
        assert!(matches!(&events[0], ProviderEvent::Started { .. }));
        assert!(matches!(&events[1], ProviderEvent::Text { text } if text == "hi"));
        assert!(state.emitted_start);
    }

    #[test]
    fn tool_call_without_prior_init_synthesizes_started() {
        let mut state = TurnState::default();
        let events = parse(
            serde_json::json!({
                "type": "tool_call", "subtype": "started", "call_id": "c1",
                "tool_call": { "readToolCall": { "args": {} } }
            }),
            &mut state,
        );
        assert!(matches!(&events[0], ProviderEvent::Started { .. }));
        assert!(matches!(&events[1], ProviderEvent::ToolStart { .. }));
    }

    #[test]
    fn tool_call_started_and_completed_map_to_tool_events() {
        let mut state = started_state();
        let events = parse(
            serde_json::json!({
                "type": "tool_call", "subtype": "started", "call_id": "c1",
                "tool_call": {
                    "readToolCall": { "args": { "path": "/tmp/a.rs" } },
                    "toolCallId": "c1", "startedAtMs": "1"
                }
            }),
            &mut state,
        );
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            ProviderEvent::ToolStart { tool_use_id, name, input }
            if tool_use_id == "c1" && name == "read" && input["path"] == "/tmp/a.rs"
        ));

        let events = parse(
            serde_json::json!({
                "type": "tool_call", "subtype": "completed", "call_id": "c1",
                "tool_call": {
                    "readToolCall": {
                        "args": { "path": "/tmp/a.rs" },
                        "result": { "success": { "content": "fn main() {}", "totalLines": 1 } }
                    },
                    "toolCallId": "c1"
                }
            }),
            &mut state,
        );
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            ProviderEvent::ToolEnd { tool_use_id, output, error, .. }
            if tool_use_id == "c1"
                && output.as_deref() == Some("fn main() {}")
                && error.is_none()
        ));
    }

    #[test]
    fn tool_call_success_without_content_pretty_prints_payload() {
        let mut state = started_state();
        let events = parse(
            serde_json::json!({
                "type": "tool_call", "subtype": "completed", "call_id": "g1",
                "tool_call": { "globToolCall": {
                    "args": { "globPattern": "**/*.rs" },
                    "result": { "success": { "files": ["a.rs"], "totalFiles": 1 } }
                }}
            }),
            &mut state,
        );
        let ProviderEvent::ToolEnd { output, error, .. } = &events[0] else {
            panic!("expected ToolEnd, got {:?}", events[0]);
        };
        assert!(error.is_none());
        let out = output.as_deref().unwrap();
        assert!(out.contains("a.rs") && out.contains("totalFiles"));
    }

    #[test]
    fn tool_call_error_result_maps_to_tool_end_error() {
        let mut state = started_state();
        let events = parse(
            serde_json::json!({
                "type": "tool_call", "subtype": "completed", "call_id": "c2",
                "tool_call": { "readToolCall": {
                    "args": { "path": "missing" },
                    "result": { "error": { "errorMessage": "File not found" } }
                }}
            }),
            &mut state,
        );
        let ProviderEvent::ToolEnd { output, error, .. } = &events[0] else {
            panic!("expected ToolEnd, got {:?}", events[0]);
        };
        assert!(output.is_none());
        assert_eq!(error.as_deref(), Some("File not found"));
    }

    #[test]
    fn shell_tool_call_is_denied_and_its_completed_frame_dropped() {
        let mut state = started_state();
        let events = parse(
            serde_json::json!({
                "type": "tool_call", "subtype": "started", "call_id": "s1",
                "tool_call": { "shellToolCall": { "args": { "command": "ls" } } }
            }),
            &mut state,
        );
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            ProviderEvent::ToolStart { name, input, .. }
            if name == "shell" && input["command"] == "ls"
        ));
        let ProviderEvent::ToolEnd { error, .. } = &events[1] else {
            panic!("expected ToolEnd, got {:?}", events[1]);
        };
        assert_eq!(
            error.as_deref(),
            Some(crate::provider::TERMINAL_TOOL_DISABLED_MSG)
        );

        let events = parse(
            serde_json::json!({
                "type": "tool_call", "subtype": "completed", "call_id": "s1",
                "tool_call": { "shellToolCall": {
                    "args": { "command": "ls" },
                    "result": { "success": { "stdout": "a.rs\n", "exitCode": 0 } }
                }}
            }),
            &mut state,
        );
        assert!(
            events.is_empty(),
            "denied tool's real result must be dropped"
        );
    }

    #[test]
    fn partial_output_deltas_stream_and_snapshot_is_swallowed() {
        let mut state = started_state();
        let mut texts = Vec::new();
        for chunk in ["I", "'ll read", " the file."] {
            let events = parse(
                serde_json::json!({
                    "type": "assistant",
                    "message": { "content": [{ "type": "text", "text": chunk }] },
                    "timestamp_ms": 1
                }),
                &mut state,
            );
            for e in events {
                if let ProviderEvent::Text { text } = e {
                    texts.push(text);
                }
            }
        }
        assert_eq!(texts.join(""), "I'll read the file.");

        // The cumulative snapshot repeats the whole segment → swallowed.
        let events = parse(
            serde_json::json!({
                "type": "assistant",
                "message": { "content": [{ "type": "text", "text": "I'll read the file." }] }
            }),
            &mut state,
        );
        assert!(events.is_empty());

        // A tool_call closes the segment; the next segment dedups afresh.
        parse(
            serde_json::json!({
                "type": "tool_call", "subtype": "started", "call_id": "c1",
                "tool_call": { "readToolCall": { "args": {} } }
            }),
            &mut state,
        );
        let events = parse(
            serde_json::json!({
                "type": "assistant",
                "message": { "content": [{ "type": "text", "text": "Done." }] }
            }),
            &mut state,
        );
        assert!(matches!(&events[0], ProviderEvent::Text { text } if text == "Done."));
    }

    #[test]
    fn result_captures_chat_id_and_emits_text_only_when_turn_was_silent() {
        // No assistant text streamed → the result text is the only copy.
        let mut state = started_state();
        let events = parse(
            serde_json::json!({
                "type": "result",
                "subtype": "success",
                "chatId": "chat-9",
                "result": "All set."
            }),
            &mut state,
        );
        assert_eq!(state.conversation_id.as_deref(), Some("chat-9"));
        assert!(matches!(&events[0], ProviderEvent::Text { text } if text == "All set."));
    }

    #[test]
    fn result_text_suppressed_after_streamed_assistant_text() {
        let mut state = started_state();
        parse(
            serde_json::json!({
                "type": "assistant",
                "message": { "content": [{ "type": "text", "text": "All set." }] }
            }),
            &mut state,
        );
        let events = parse(
            serde_json::json!({ "type": "result", "subtype": "success", "result": "All set." }),
            &mut state,
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, ProviderEvent::Text { .. })),
            "result echo must not duplicate streamed text"
        );
    }

    #[test]
    fn result_usage_emits_usage_event() {
        let mut state = started_state();
        state.model_name = Some("Sonnet 4.5".into());
        let events = parse(
            serde_json::json!({
                "type": "result", "subtype": "success", "result": "",
                "session_id": "chat-1",
                "usage": { "inputTokens": 7, "outputTokens": 92,
                            "cacheReadTokens": 100, "cacheWriteTokens": 900 }
            }),
            &mut state,
        );
        let usage = events
            .iter()
            .find(|e| matches!(e, ProviderEvent::Usage { .. }))
            .expect("usage event");
        let ProviderEvent::Usage {
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_creation_tokens,
            total_tokens,
            context_tokens,
            model,
            turn_seq,
        } = usage
        else {
            unreachable!();
        };
        assert_eq!(*input_tokens, 7);
        assert_eq!(*output_tokens, 92);
        assert_eq!(*cache_read_tokens, 100);
        assert_eq!(*cache_creation_tokens, 900);
        assert_eq!(*context_tokens, 1007);
        assert_eq!(*total_tokens, 1099);
        assert_eq!(model.as_deref(), Some("Sonnet 4.5"));
        assert!(turn_seq.is_none());
    }

    #[test]
    fn oversized_tool_output_is_capped() {
        let big = "x".repeat(MAX_TOOL_OUTPUT_BYTES + 10);
        let mut state = started_state();
        let events = parse(
            serde_json::json!({
                "type": "tool_call", "subtype": "completed", "call_id": "c9",
                "tool_call": { "readToolCall": {
                    "result": { "success": { "content": big.clone() } }
                }}
            }),
            &mut state,
        );
        let ProviderEvent::ToolEnd { output, .. } = &events[0] else {
            panic!("expected ToolEnd, got {:?}", events[0]);
        };
        let out = output.as_deref().unwrap();
        assert!(out.len() <= MAX_TOOL_OUTPUT_BYTES + 64);
        assert!(out.contains("truncated"));
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
