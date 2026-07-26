//! Stateful Claude CLI `--output-format stream-json` parser.
//!
//! The CLI emits JSON objects in a few shapes (`system`, `assistant`,
//! `stream_event` envelopes around raw Anthropic stream events, `result`,
//! `user`); each one becomes zero or more `ProviderEvent`s. [`ParserState`]
//! carries the cross-call cursors (conversation_id, model_name,
//! emitted_start) plus the per-block coalescing and snapshot-dedupe state
//! for partial messages.

use crate::provider::stream::{ProviderEvent, ToolImage};

/// Coalescing threshold for streamed deltas. Every emitted
/// `ProviderEvent::Text`/`Thinking` becomes a DB row plus a WS broadcast,
/// so per-token deltas are buffered and flushed in chunks of at least this
/// many bytes (or whatever remains when the block ends).
const DELTA_FLUSH_BYTES: usize = 200;

/// Kind of streamable content block whose deltas we coalesce and dedupe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
}

impl BlockKind {
    fn event(self, text: String) -> ProviderEvent {
        match self {
            BlockKind::Text => ProviderEvent::Text { text },
            BlockKind::Thinking => ProviderEvent::Thinking { text },
        }
    }
}

/// Streaming progress of one content block: `emitted` has reached the event
/// stream, `pending` is buffered awaiting a coalescing flush. The entry
/// lives until the block's `assistant` snapshot consumes it, which is how
/// snapshot text avoids being re-emitted on top of streamed deltas.
struct BlockProgress {
    kind: BlockKind,
    emitted: String,
    pending: String,
}

/// Mutable parser state carried across [`parse_stream_json`] calls.
///
/// `conversation_id` and `model_name` persist for the life of the child
/// process; `emitted_start` and the block-tracking state are per-turn and
/// reset via [`ParserState::reset_turn`] when a `result` event settles the
/// turn (the live CLI re-emits `system.init` on every turn of a long-lived
/// child, so the next turn produces a fresh `Started`).
pub(super) struct ParserState {
    pub(super) conversation_id: Option<String>,
    pub(super) model_name: Option<String>,
    pub(super) emitted_start: bool,
    blocks: std::collections::VecDeque<BlockProgress>,
}

impl ParserState {
    pub(super) fn new() -> Self {
        ParserState {
            conversation_id: None,
            model_name: None,
            emitted_start: false,
            blocks: std::collections::VecDeque::new(),
        }
    }

    /// Reset per-turn state after a `result` event. The discovered
    /// conversation_id and model_name survive for the next turn.
    pub(super) fn reset_turn(&mut self) {
        self.emitted_start = false;
        self.blocks.clear();
    }

    fn on_block_start(&mut self, kind: BlockKind) {
        self.blocks.push_back(BlockProgress {
            kind,
            emitted: String::new(),
            pending: String::new(),
        });
    }

    /// Buffer a streamed delta; returns a coalesced event once the buffer
    /// crosses the flush threshold. Deltas arriving without a tracked block
    /// (older CLIs sending bare deltas) pass through unbuffered.
    fn on_delta(&mut self, kind: BlockKind, text: &str) -> Option<ProviderEvent> {
        if text.is_empty() {
            return None;
        }
        let block = match self.blocks.back_mut() {
            Some(b) if b.kind == kind => b,
            _ => return Some(kind.event(text.to_string())),
        };
        block.pending.push_str(text);
        if block.pending.len() < DELTA_FLUSH_BYTES {
            return None;
        }
        let chunk = std::mem::take(&mut block.pending);
        block.emitted.push_str(&chunk);
        Some(kind.event(chunk))
    }

    /// Flush whatever the open block still has buffered. The block entry
    /// stays queued for its snapshot (which on the live CLI arrives just
    /// BEFORE content_block_stop and has removed it already — then this is
    /// a no-op).
    fn on_block_stop(&mut self) -> Option<ProviderEvent> {
        let block = self.blocks.back_mut()?;
        if block.pending.is_empty() {
            return None;
        }
        let chunk = std::mem::take(&mut block.pending);
        block.emitted.push_str(&chunk);
        Some(block.kind.event(chunk))
    }

    /// A per-block `assistant` snapshot arrived with the block's full text.
    /// Emit only the part streaming hasn't already emitted: everything, when
    /// partials are off and nothing streamed; nothing, when the deltas
    /// covered the whole block.
    fn consume_snapshot(&mut self, kind: BlockKind, full_text: &str) -> Option<ProviderEvent> {
        let Some(idx) = self.blocks.iter().position(|b| b.kind == kind) else {
            if full_text.is_empty() {
                return None;
            }
            return Some(kind.event(full_text.to_string()));
        };
        let block = self.blocks.remove(idx)?;
        match full_text.strip_prefix(&block.emitted) {
            Some(rest) if !rest.is_empty() => Some(kind.event(rest.to_string())),
            Some(_) => None,
            None => {
                // Snapshot disagrees with what already streamed — emitting it
                // would duplicate the block, so drop it.
                tracing::warn!("assistant snapshot diverged from streamed deltas; suppressed");
                None
            }
        }
    }
}
/// Extract the textual output and any images from a `tool_result` block's
/// `content`.
///
/// The CLI gives `content` in one of two shapes:
/// - a bare string (the common case — a tool that returned only text), or
/// - an array of content blocks, each `{"type": "text"|"image", …}`.
///
/// Tools that return images (Playwright MCP `browser_take_screenshot`, any
/// image-returning MCP server) take the array form. We previously read only
/// the string shape and dropped the array entirely, losing screenshots. We
/// now concatenate every text block into `output` and pull each image block
/// out as a [`ToolImage`].
///
/// Image blocks come in two flavours we tolerate:
/// - Anthropic envelope: `{"type":"image","source":{"type":"base64",
///   "media_type":"image/png","data":"…"}}`
/// - raw MCP: `{"type":"image","data":"…","mimeType":"image/png"}`
fn extract_tool_result(content: Option<&serde_json::Value>) -> (Option<String>, Vec<ToolImage>) {
    let Some(content) = content else {
        return (None, Vec::new());
    };

    // String shape: text only, no images.
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
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    if !t.is_empty() {
                        texts.push(t.to_string());
                    }
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

/// Pull a [`ToolImage`] out of a single `{"type":"image", …}` block,
/// tolerating both the Anthropic `source.{media_type,data}` envelope and the
/// raw MCP `{mimeType,data}` shape. Returns `None` if the base64 payload is
/// missing.
fn parse_image_block(block: &serde_json::Value) -> Option<ToolImage> {
    // Anthropic envelope.
    if let Some(source) = block.get("source") {
        if let Some(data) = source.get("data").and_then(|v| v.as_str()) {
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
    }
    // Raw MCP shape.
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

/// Parse a single JSON line from Claude CLI stream-json output into zero or
/// more `ProviderEvent` values.
///
/// The Claude CLI `--output-format stream-json --verbose
/// --include-partial-messages` emits JSON objects that can take several
/// forms. We handle the common patterns:
///
/// - `{"type":"system","subtype":"init",...}` - initialization with model info
/// - `{"type":"stream_event","event":{...}}` - envelope around a raw
///   Anthropic stream event (`message_start`, `content_block_start`,
///   `content_block_delta`, `content_block_stop`, `message_delta`,
///   `message_stop`); unwrapped and re-parsed
/// - `{"type":"assistant","message":{...}}` - per-block message snapshots
/// - `{"type":"user","message":{...}}` - tool results
/// - `{"type":"result",...}` - final result with conversation_id
/// - bare `content_block_*` / `message_*` types from older CLIs
///
/// Streamed text/thinking deltas are coalesced in [`ParserState`] and
/// deduplicated against the per-block `assistant` snapshots, so each
/// block's content reaches the event stream exactly once.
pub(super) fn parse_stream_json(
    json: &serde_json::Value,
    state: &mut ParserState,
) -> Vec<ProviderEvent> {
    let mut events = Vec::new();

    let msg_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");

    // With --include-partial-messages the CLI wraps raw Anthropic stream
    // events in a `{"type":"stream_event","event":{...}}` envelope.
    if msg_type == "stream_event" {
        return match json.get("event") {
            Some(inner) => parse_stream_json(inner, state),
            None => events,
        };
    }

    match msg_type {
        // ── system frames (init + notices) ───────────────────────
        "system" => {
            let subtype = json.get("subtype").and_then(|v| v.as_str()).unwrap_or("");

            if subtype == "init" {
                if let Some(model) = json.get("model").and_then(|v| v.as_str()) {
                    state.model_name = Some(model.to_string());
                }
                // CLI uses "session_id" as the resumable conversation identifier
                if let Some(cid) = json
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .or_else(|| json.get("conversation_id").and_then(|v| v.as_str()))
                {
                    state.conversation_id = Some(cid.to_string());
                }
                if !state.emitted_start {
                    state.emitted_start = true;
                    events.push(ProviderEvent::Started {
                        model: state.model_name.clone().unwrap_or_else(|| "unknown".into()),
                        conversation_id: state.conversation_id.clone(),
                        metadata: json.clone(),
                    });
                }
            } else if let Some(text) = system_notice_text(subtype, json) {
                // Non-init system frames (`compact_boundary`, status, …) used
                // to fall on the floor, so the UI never learned the CLI had
                // compacted mid-session. Surface them as chat-visible rows.
                events.push(ProviderEvent::System {
                    text,
                    subtype: subtype.to_string(),
                    detail: json.clone(),
                });
            }
        }

        // ── message_start ────────────────────────────────────────
        "message_start" => {
            if let Some(msg) = json.get("message") {
                if let Some(model) = msg.get("model").and_then(|v| v.as_str()) {
                    state.model_name = Some(model.to_string());
                }
                if let Some(cid) = msg.get("id").and_then(|v| v.as_str()) {
                    // The message id can serve as a conversation identifier
                    if state.conversation_id.is_none() {
                        state.conversation_id = Some(cid.to_string());
                    }
                }
                if !state.emitted_start {
                    state.emitted_start = true;
                    events.push(ProviderEvent::Started {
                        model: state.model_name.clone().unwrap_or_else(|| "unknown".into()),
                        conversation_id: state.conversation_id.clone(),
                        metadata: json.clone(),
                    });
                }
            }
        }

        // ── content_block_start ──────────────────────────────────
        "content_block_start" => {
            if let Some(block) = json.get("content_block") {
                match block.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                    "text" => state.on_block_start(BlockKind::Text),
                    "thinking" => state.on_block_start(BlockKind::Thinking),
                    // tool_use: no event here — the input hasn't streamed
                    // yet (it arrives via input_json_delta), and the
                    // per-block `assistant` snapshot carries the complete
                    // call, so ToolStart is emitted from there.
                    _ => {}
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
                            events.extend(state.on_delta(BlockKind::Text, text));
                        }
                    }
                    "thinking_delta" => {
                        if let Some(text) = delta.get("thinking").and_then(|v| v.as_str()) {
                            events.extend(state.on_delta(BlockKind::Thinking, text));
                        }
                    }
                    "input_json_delta" => {
                        // Partial JSON for a tool_use input. Not surfaced:
                        // the complete input arrives with the per-block
                        // `assistant` snapshot, which is what emits ToolStart.
                    }
                    // signature_delta and friends carry no user-visible text
                    _ => {}
                }
            }
        }

        // ── content_block_stop ───────────────────────────────────
        "content_block_stop" => {
            // Flush any coalesced text still buffered for the block. No
            // ToolEnd here: a tool_use block stopping only means its input
            // finished streaming — the result arrives later as a `user`
            // tool_result.
            events.extend(state.on_block_stop());
        }

        // ── assistant message snapshot ───────────────────────────
        "assistant" => {
            if let Some(msg) = json.get("message") {
                if let Some(content) = msg.get("content").and_then(|v| v.as_array()) {
                    for block in content {
                        let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        match block_type {
                            // With partials on, the CLI sends one `assistant`
                            // snapshot per completed content block; text and
                            // thinking already streamed as deltas must not be
                            // re-emitted, so snapshots go through the
                            // dedupe-consume path.
                            "text" => {
                                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                                    events.extend(state.consume_snapshot(BlockKind::Text, text));
                                }
                            }
                            "thinking" => {
                                if let Some(text) = block.get("thinking").and_then(|v| v.as_str()) {
                                    events
                                        .extend(state.consume_snapshot(BlockKind::Thinking, text));
                                }
                            }
                            "redacted_thinking" => {
                                events.push(ProviderEvent::Thinking {
                                    text: "[redacted thinking]".to_string(),
                                });
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
                                let (output, images) = extract_tool_result(block.get("content"));
                                let is_error = block
                                    .get("is_error")
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false);
                                let error = if is_error { output.clone() } else { None };
                                events.push(ProviderEvent::ToolEnd {
                                    tool_use_id: tool_id,
                                    output: if is_error { None } else { output },
                                    error,
                                    images,
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
                            let (output, images) = extract_tool_result(block.get("content"));
                            let is_error = block
                                .get("is_error")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            let error = if is_error { output.clone() } else { None };
                            events.push(ProviderEvent::ToolEnd {
                                tool_use_id: tool_id,
                                output: if is_error { None } else { output },
                                error,
                                images,
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
                state.conversation_id = Some(cid.to_string());
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

/// Chat label for a non-`init` `system` frame, or `None` when the frame
/// carries nothing worth showing. The CLI uses these for out-of-band
/// notices — most importantly `compact_boundary`, the only signal that it
/// compacted the conversation mid-session.
fn system_notice_text(subtype: &str, json: &serde_json::Value) -> Option<String> {
    match subtype {
        "" => None,
        "compact_boundary" => {
            let meta = json.get("compact_metadata");
            let trigger = meta
                .and_then(|m| m.get("trigger"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let mut text = format!("Claude CLI compacted the conversation (trigger: {trigger}");
            if let Some(pre) = meta
                .and_then(|m| m.get("pre_tokens"))
                .and_then(|v| v.as_i64())
            {
                text.push_str(&format!(", {pre} tokens before"));
            }
            text.push(')');
            Some(text)
        }
        _ => {
            let detail = json
                .get("message")
                .or_else(|| json.get("text"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            Some(match detail {
                Some(d) => format!("Claude CLI [{subtype}]: {d}"),
                None => format!("Claude CLI: {subtype}"),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started_state() -> ParserState {
        let mut state = ParserState::new();
        state.emitted_start = true;
        state
    }

    #[test]
    fn test_parse_system_init() {
        let json = serde_json::json!({
            "type": "system",
            "subtype": "init",
            "model": "claude-sonnet-4-20250514",
            "session_id": "conv-abc123"
        });

        let mut state = ParserState::new();
        let events = parse_stream_json(&json, &mut state);

        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], ProviderEvent::Started { model, .. } if model == "claude-sonnet-4-20250514")
        );
        assert_eq!(state.conversation_id.as_deref(), Some("conv-abc123"));
        assert!(state.emitted_start);
    }

    #[test]
    fn test_parse_content_block_delta_text() {
        // A bare delta with no tracked block (older CLI without the
        // stream_event envelope) passes straight through, uncoalesced.
        let json = serde_json::json!({
            "type": "content_block_delta",
            "delta": {
                "type": "text_delta",
                "text": "Hello world"
            }
        });

        let mut state = started_state();
        let events = parse_stream_json(&json, &mut state);

        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ProviderEvent::Text { text } if text == "Hello world"));
    }

    #[test]
    fn tool_use_block_start_emits_nothing_until_snapshot() {
        let mut state = started_state();

        // content_block_start for a tool has no input yet — no ToolStart.
        let start = serde_json::json!({
            "type": "stream_event",
            "event": {
                "type": "content_block_start",
                "index": 1,
                "content_block": { "type": "tool_use", "id": "tool_123", "name": "Read" }
            }
        });
        assert!(parse_stream_json(&start, &mut state).is_empty());

        // The input streams as partial JSON — still nothing to emit.
        let delta = serde_json::json!({
            "type": "stream_event",
            "event": {
                "type": "content_block_delta",
                "index": 1,
                "delta": { "type": "input_json_delta", "partial_json": "{\"file_path\":" }
            }
        });
        assert!(parse_stream_json(&delta, &mut state).is_empty());

        // No premature ToolEnd when the input finishes streaming: the real
        // result arrives later as a `user` tool_result.
        let stop = serde_json::json!({
            "type": "stream_event",
            "event": { "type": "content_block_stop", "index": 1 }
        });
        assert!(parse_stream_json(&stop, &mut state).is_empty());

        // The per-block assistant snapshot carries the complete call and is
        // the single ToolStart source.
        let snapshot = serde_json::json!({
            "type": "assistant",
            "message": { "content": [{
                "type": "tool_use",
                "id": "tool_123",
                "name": "Read",
                "input": { "file_path": "x.rs" }
            }]}
        });
        let events = parse_stream_json(&snapshot, &mut state);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            ProviderEvent::ToolStart { tool_use_id, name, input }
            if tool_use_id == "tool_123" && name == "Read" && input["file_path"] == "x.rs"
        ));
    }

    #[test]
    fn test_tool_result_array_extracts_image_anthropic_shape() {
        // A Playwright MCP screenshot arrives as a `user` tool_result whose
        // `content` is an array of blocks, including an Anthropic-enveloped
        // image. The parser must surface the image (previously dropped) and
        // keep the accompanying text as output.
        let json = serde_json::json!({
            "type": "user",
            "message": {
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tool_shot",
                    "content": [
                        { "type": "text", "text": "Took the screenshot" },
                        { "type": "image", "source": {
                            "type": "base64",
                            "media_type": "image/png",
                            "data": "QUJD"
                        }}
                    ]
                }]
            }
        });

        let mut state = started_state();
        let events = parse_stream_json(&json, &mut state);

        assert_eq!(events.len(), 1);
        let ProviderEvent::ToolEnd {
            tool_use_id,
            output,
            error,
            images,
        } = &events[0]
        else {
            panic!("expected ToolEnd, got {:?}", events[0]);
        };
        assert_eq!(tool_use_id, "tool_shot");
        assert_eq!(output.as_deref(), Some("Took the screenshot"));
        assert!(error.is_none());
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime_type, "image/png");
        assert_eq!(images[0].data_base64, "QUJD");
    }

    #[test]
    fn test_tool_result_array_extracts_image_mcp_shape() {
        // Raw MCP image blocks use `{mimeType, data}` rather than the
        // Anthropic `source` envelope. Both must be tolerated.
        let json = serde_json::json!({
            "type": "user",
            "message": {
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tool_mcp",
                    "content": [
                        { "type": "image", "data": "WFla", "mimeType": "image/jpeg" }
                    ]
                }]
            }
        });

        let mut state = started_state();
        let events = parse_stream_json(&json, &mut state);

        assert_eq!(events.len(), 1);
        let ProviderEvent::ToolEnd { images, output, .. } = &events[0] else {
            panic!("expected ToolEnd, got {:?}", events[0]);
        };
        // No text blocks → no output, just the image.
        assert!(output.is_none());
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime_type, "image/jpeg");
        assert_eq!(images[0].data_base64, "WFla");
    }

    #[test]
    fn test_tool_result_string_shape_has_no_images() {
        // The common case — a plain text tool result — must keep working
        // exactly as before: output set, images empty.
        let json = serde_json::json!({
            "type": "user",
            "message": {
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tool_txt",
                    "content": "plain output"
                }]
            }
        });

        let mut state = started_state();
        let events = parse_stream_json(&json, &mut state);

        assert_eq!(events.len(), 1);
        let ProviderEvent::ToolEnd { output, images, .. } = &events[0] else {
            panic!("expected ToolEnd, got {:?}", events[0]);
        };
        assert_eq!(output.as_deref(), Some("plain output"));
        assert!(images.is_empty());
    }

    #[test]
    fn test_parse_result_captures_conversation_id() {
        let json = serde_json::json!({
            "type": "result",
            "session_id": "conv-final-456"
        });

        let mut state = started_state();
        let events = parse_stream_json(&json, &mut state);

        assert!(events.is_empty());
        assert_eq!(state.conversation_id.as_deref(), Some("conv-final-456"));
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

        let mut state = ParserState::new();
        let events = parse_stream_json(&json, &mut state);

        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], ProviderEvent::Started { model, .. } if model == "claude-sonnet-4-20250514")
        );
        assert!(state.emitted_start);
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

        let mut state = started_state();
        let events = parse_stream_json(&json, &mut state);
        assert!(events.is_empty());
    }

    #[test]
    fn test_parse_assistant_snapshot() {
        // Nothing streamed beforehand (partials off) — the snapshot emits
        // its full content, exactly the pre-partial behavior.
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

        let mut state = started_state();
        let events = parse_stream_json(&json, &mut state);

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

        let mut state = started_state(); // already started
        let events = parse_stream_json(&json, &mut state);

        // Should not emit another Started event
        assert!(events.is_empty());
        // But should still update model_name
        assert_eq!(state.model_name.as_deref(), Some("opus"));
    }

    #[test]
    fn test_parse_thinking_blocks() {
        let mut state = started_state();

        // Bare streaming delta form (no tracked block) passes through.
        let delta = serde_json::json!({
            "type": "content_block_delta",
            "delta": { "type": "thinking_delta", "thinking": "hmm, " }
        });
        let events = parse_stream_json(&delta, &mut state);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ProviderEvent::Thinking { text } if text == "hmm, "));

        // Assistant snapshot form, including the redacted variant.
        let snapshot = serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                { "type": "thinking", "thinking": "reasoning here", "signature": "sig" },
                { "type": "redacted_thinking", "data": "opaque" },
                { "type": "text", "text": "answer" }
            ]}
        });
        let events = parse_stream_json(&snapshot, &mut state);
        assert_eq!(events.len(), 3, "got: {events:?}");
        assert!(matches!(&events[0], ProviderEvent::Thinking { text } if text == "reasoning here"));
        assert!(
            matches!(&events[1], ProviderEvent::Thinking { text } if text == "[redacted thinking]")
        );
        assert!(matches!(&events[2], ProviderEvent::Text { text } if text == "answer"));
    }

    #[test]
    fn stream_event_envelope_is_unwrapped() {
        let mut state = ParserState::new();
        let json = serde_json::json!({
            "type": "stream_event",
            "event": {
                "type": "message_start",
                "message": { "id": "msg-1", "model": "claude-fable-5" }
            }
        });
        let events = parse_stream_json(&json, &mut state);
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], ProviderEvent::Started { model, .. } if model == "claude-fable-5")
        );
        assert_eq!(state.model_name.as_deref(), Some("claude-fable-5"));
    }

    fn envelope(event: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "type": "stream_event", "event": event })
    }

    fn text_delta(text: &str) -> serde_json::Value {
        envelope(serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": text }
        }))
    }

    #[test]
    fn streamed_text_coalesces_and_flushes_on_block_end() {
        let mut state = started_state();
        let start = envelope(serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "text", "text": "" }
        }));
        assert!(parse_stream_json(&start, &mut state).is_empty());

        // Small deltas buffer instead of emitting one event per token.
        assert!(parse_stream_json(&text_delta("Hello "), &mut state).is_empty());

        // Crossing the threshold flushes everything buffered as ONE event.
        let big = "x".repeat(DELTA_FLUSH_BYTES);
        let events = parse_stream_json(&text_delta(&big), &mut state);
        assert_eq!(events.len(), 1);
        let expected = format!("Hello {big}");
        assert!(matches!(&events[0], ProviderEvent::Text { text } if *text == expected));

        // A trailing fragment flushes when the block ends.
        assert!(parse_stream_json(&text_delta("tail"), &mut state).is_empty());
        let stop = envelope(serde_json::json!({ "type": "content_block_stop", "index": 0 }));
        let events = parse_stream_json(&stop, &mut state);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ProviderEvent::Text { text } if text == "tail"));
    }

    #[test]
    fn assistant_snapshot_dedupes_streamed_text() {
        let mut state = started_state();
        let start = envelope(serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "text", "text": "" }
        }));
        assert!(parse_stream_json(&start, &mut state).is_empty());

        // Big delta flushes immediately (already emitted to the stream)…
        let big = "x".repeat(DELTA_FLUSH_BYTES);
        assert_eq!(parse_stream_json(&text_delta(&big), &mut state).len(), 1);
        // …small tail stays buffered.
        assert!(parse_stream_json(&text_delta(" tail"), &mut state).is_empty());

        // The per-block snapshot (arrives BEFORE content_block_stop on the
        // live CLI) carries the full block; only the un-emitted remainder
        // may come out — never the already-streamed prefix again.
        let snapshot = serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                { "type": "text", "text": format!("{big} tail") }
            ]}
        });
        let events = parse_stream_json(&snapshot, &mut state);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ProviderEvent::Text { text } if text == " tail"));

        // Block already consumed by the snapshot — stop emits nothing.
        let stop = envelope(serde_json::json!({ "type": "content_block_stop", "index": 0 }));
        assert!(parse_stream_json(&stop, &mut state).is_empty());
    }

    #[test]
    fn assistant_snapshot_dedupes_streamed_thinking() {
        let mut state = started_state();
        let start = envelope(serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "thinking", "thinking": "" }
        }));
        assert!(parse_stream_json(&start, &mut state).is_empty());

        let delta = envelope(serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "thinking_delta", "thinking": "hmm, " }
        }));
        assert!(parse_stream_json(&delta, &mut state).is_empty());
        let delta = envelope(serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "thinking_delta", "thinking": "done." }
        }));
        assert!(parse_stream_json(&delta, &mut state).is_empty());

        // Nothing was flushed yet, so the snapshot emits the whole block —
        // exactly once.
        let snapshot = serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                { "type": "thinking", "thinking": "hmm, done.", "signature": "sig" }
            ]}
        });
        let events = parse_stream_json(&snapshot, &mut state);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ProviderEvent::Thinking { text } if text == "hmm, done."));

        let stop = envelope(serde_json::json!({ "type": "content_block_stop", "index": 0 }));
        assert!(parse_stream_json(&stop, &mut state).is_empty());
    }

    #[test]
    fn init_reemits_started_after_reset_turn() {
        let mut state = ParserState::new();
        let init = serde_json::json!({
            "type": "system",
            "subtype": "init",
            "model": "opus",
            "session_id": "conv-1"
        });
        assert_eq!(parse_stream_json(&init, &mut state).len(), 1);
        assert!(parse_stream_json(&init, &mut state).is_empty());

        // The stream loop resets per-turn state on each `result`; the live
        // CLI re-emits system.init at the start of every subsequent turn.
        state.reset_turn();
        let events = parse_stream_json(&init, &mut state);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ProviderEvent::Started { .. }));
        assert_eq!(state.conversation_id.as_deref(), Some("conv-1"));
    }

    #[test]
    fn compact_boundary_emits_system_event() {
        let json = serde_json::json!({
            "type": "system",
            "subtype": "compact_boundary",
            "compact_metadata": { "trigger": "auto", "pre_tokens": 145000 },
        });

        let mut state = started_state();
        let events = parse_stream_json(&json, &mut state);

        assert_eq!(events.len(), 1);
        let ProviderEvent::System {
            text,
            subtype,
            detail,
        } = &events[0]
        else {
            panic!("expected System, got {:?}", events[0]);
        };
        assert_eq!(subtype, "compact_boundary");
        assert_eq!(
            text,
            "Claude CLI compacted the conversation (trigger: auto, 145000 tokens before)"
        );
        assert_eq!(detail["compact_metadata"]["trigger"], "auto");
    }

    #[test]
    fn unknown_system_subtype_emits_labelled_system_event() {
        let json = serde_json::json!({
            "type": "system",
            "subtype": "status",
            "message": "rate limit resets in 4m",
        });

        let mut state = started_state();
        let events = parse_stream_json(&json, &mut state);

        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            ProviderEvent::System { text, .. }
                if text == "Claude CLI [status]: rate limit resets in 4m"
        ));
    }

    #[test]
    fn system_init_emits_no_notice() {
        let json = serde_json::json!({
            "type": "system",
            "subtype": "init",
            "model": "opus",
            "session_id": "conv-1",
        });

        let mut state = started_state();
        assert!(parse_stream_json(&json, &mut state).is_empty());
    }

    #[test]
    fn subtypeless_system_frame_is_ignored() {
        let json = serde_json::json!({ "type": "system" });
        let mut state = started_state();
        assert!(parse_stream_json(&json, &mut state).is_empty());
    }
}
