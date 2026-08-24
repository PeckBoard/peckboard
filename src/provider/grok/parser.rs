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
//! Token counts: grok 1.0 reports two distinct figures that must not be
//! mixed. The mid-stream `usage` frame is **one per model response** (a
//! tool-loop round). The `end` frame's `usage` / `modelUsage` **sums**
//! every round in the prompt, including finished subagents. Billed slices
//! therefore come from `end`; context-window occupancy is the last
//! per-response snapshot (the window at end of turn). Using the end-frame
//! sum as occupancy inflates the session gauge by `num_turns` — the same
//! class of bug Claude's `UsageTracker` documents. [`UsageTracker`] holds
//! the per-response snapshots across lines of one turn. Earlier CLIs
//! (0.2.x) reported no counts anywhere, so an `end` frame without usage
//! still parses fine.
//!
//! The exact tool-event shape isn't formally specified, so every accessor is
//! defensive: an unrecognised shape yields no events rather than an error.

use crate::provider::stream::ProviderEvent;

/// Per-turn token accounting for one grok process. Observe every stdout
/// line; settle on the `end` frame. One process = one turn, so there is
/// no cross-turn cumulative delta (unlike Claude's long-lived CLI).
#[derive(Default)]
pub(super) struct UsageTracker {
    /// Per-response snapshots from mid-stream `usage` frames, in order.
    responses: Vec<UsageSlices>,
}

/// The four billed token slices, same shape as Claude's tracker.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct UsageSlices {
    input: i64,
    output: i64,
    cache_read: i64,
    cache_creation: i64,
}

impl UsageSlices {
    fn is_zero(self) -> bool {
        self == Self::default()
    }

    fn context(self) -> i64 {
        self.input + self.cache_read + self.cache_creation
    }

    fn total(self) -> i64 {
        self.context() + self.output
    }

    /// Parse a usage object, tolerating headless snake_case, modelUsage
    /// camelCase, and ACP `cachedReadTokens` / `cacheCreationTokens`.
    fn from_obj(usage: &serde_json::Value) -> Self {
        let field = |keys: &[&str]| {
            keys.iter()
                .find_map(|k| usage.get(*k).and_then(|v| v.as_i64()))
                .unwrap_or(0)
        };
        Self {
            input: field(&["input_tokens", "inputTokens"]),
            output: field(&["output_tokens", "outputTokens"]),
            cache_read: field(&[
                "cache_read_input_tokens",
                "cacheReadInputTokens",
                "cachedReadTokens",
            ]),
            cache_creation: field(&[
                "cache_creation_input_tokens",
                "cacheCreationInputTokens",
                "cacheCreationTokens",
            ]),
        }
    }
}

impl UsageTracker {
    /// Record a mid-stream `usage` frame. No-op on any other type.
    fn observe(&mut self, json: &serde_json::Value) {
        if json.get("type").and_then(|v| v.as_str()) != Some("usage") {
            return;
        }
        let Some(obj) = json.get("usage") else {
            return;
        };
        let slices = UsageSlices::from_obj(obj);
        if !slices.is_zero() {
            self.responses.push(slices);
        }
    }

    /// Occupancy at end of turn: last positive per-response context, so a
    /// mid-turn compact is reflected. `None` when no usage frames arrived.
    fn occupancy(&self) -> Option<i64> {
        self.responses
            .iter()
            .rev()
            .map(|s| s.context())
            .find(|&c| c > 0)
    }

    /// Build `Usage` events from an `end` frame. Prefers per-model
    /// `modelUsage` (one event per model so subagents roll up); falls back
    /// to the flat `usage` rollup. Context occupancy is the last
    /// per-response snapshot when any exist; otherwise the billed
    /// occupancy (correct for a single-round turn).
    fn on_end(&mut self, json: &serde_json::Value, main_model: Option<&str>) -> Vec<ProviderEvent> {
        let occupancy = self.occupancy();
        self.responses.clear();

        let to_event = |slices: UsageSlices, model: Option<String>, ctx: i64| {
            if slices.is_zero() {
                return None;
            }
            Some(ProviderEvent::Usage {
                input_tokens: slices.input,
                output_tokens: slices.output,
                cache_read_tokens: slices.cache_read,
                cache_creation_tokens: slices.cache_creation,
                total_tokens: slices.total(),
                context_tokens: ctx,
                model,
                turn_seq: None,
            })
        };

        if let Some(models) = json.get("modelUsage").and_then(|v| v.as_object())
            && !models.is_empty()
        {
            let mut out: Vec<(UsageSlices, Option<String>)> = models
                .iter()
                .filter_map(|(model, u)| {
                    let slices = UsageSlices::from_obj(u);
                    if slices.is_zero() {
                        None
                    } else {
                        Some((slices, Some(model.clone())))
                    }
                })
                .collect();
            if out.is_empty() {
                return Vec::new();
            }
            let ctx_idx = out
                .iter()
                .position(|(_, m)| m.as_deref() == main_model)
                .unwrap_or_else(|| {
                    out.iter()
                        .enumerate()
                        .max_by_key(|(_, (s, _))| s.context())
                        .map(|(i, _)| i)
                        .unwrap_or(0)
                });
            return out
                .drain(..)
                .enumerate()
                .filter_map(|(i, (slices, model))| {
                    let ctx = if i == ctx_idx {
                        occupancy.unwrap_or_else(|| slices.context())
                    } else {
                        0
                    };
                    to_event(slices, model, ctx)
                })
                .collect();
        }

        let Some(u) = json.get("usage") else {
            return Vec::new();
        };
        let slices = UsageSlices::from_obj(u);
        let ctx = occupancy.unwrap_or_else(|| slices.context());
        to_event(slices, main_model.map(str::to_string), ctx)
            .into_iter()
            .collect()
    }
}

/// Parse one JSON line of grok streaming-json into provider events, updating
/// `conversation_id` from any `sessionId` the line carries (the `end` frame
/// always does). `usage` is the per-turn tracker; `main_model` is the
/// session's bare CLI model id, used to pin occupancy on the right
/// `modelUsage` row.
pub(super) fn parse_stream_json(
    json: &serde_json::Value,
    conversation_id: &mut Option<String>,
    usage: &mut UsageTracker,
    main_model: Option<&str>,
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
            let raw_name = json
                .get("name")
                .or_else(|| json.get("tool"))
                .or_else(|| json.get("toolName"))
                .and_then(|v| v.as_str())
                .unwrap_or("tool")
                .to_string();
            let raw_input = json
                .get("rawInput")
                .or_else(|| json.get("input"))
                .or_else(|| json.get("args"))
                .or_else(|| json.get("arguments"))
                .or_else(|| json.get("parameters"))
                .or_else(|| json.get("params"))
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            // Grok routes MCP through `use_tool` (and older `CallMcpTool`)
            // with the real identity in `tool_name` = `server__tool`. Unwrap
            // to `mcp__<server>__<tool>` — the shape Claude and cursor emit,
            // so the shared tool-display layer labels the row.
            let (name, input) = unwrap_mcp_use_tool(&raw_name, raw_input);
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
        // billed token rollup; occupancy comes from observed `usage` frames.
        // The run loop emits the terminal Completed itself.
        "end" => events.extend(usage.on_end(json, main_model)),

        // Per-response snapshot: record for occupancy, emit nothing. Emitting
        // here would double-count billed tokens against the `end` rollup.
        // `error` is inspected by the run loop for a crash reason, and
        // `available_commands` is startup chatter.
        "usage" => usage.observe(json),

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

/// Real name + arguments of a grok MCP dispatch (`use_tool` / `CallMcpTool`).
/// Grok's MCP tools are `server__tool`; Claude and cursor emit
/// `mcp__<server>__<tool>`. Rewrite so the chat row matches the other
/// providers. Unrecognised shapes stay `use_tool` with the original args.
fn unwrap_mcp_use_tool(raw_name: &str, input: serde_json::Value) -> (String, serde_json::Value) {
    match raw_name {
        "use_tool" | "CallMcpTool" | "mcp" => {}
        _ => return (raw_name.to_string(), input),
    }
    let field = |key: &str| {
        input
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
    };
    let Some(qualified) = field("tool_name").or_else(|| field("name")) else {
        return (raw_name.to_string(), input);
    };
    let display = if qualified.starts_with("mcp__") {
        qualified.to_string()
    } else if qualified.contains("__") {
        format!("mcp__{qualified}")
    } else {
        format!("mcp__peckboard__{qualified}")
    };
    let inner = input
        .get("tool_input")
        .or_else(|| input.get("arguments"))
        .or_else(|| input.get("args"))
        .cloned()
        .unwrap_or(input);
    (display, inner)
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
        let mut usage = UsageTracker::default();
        parse_stream_json(&json, conv, &mut usage, None)
    }

    fn parse_turn(lines: &[serde_json::Value], main_model: Option<&str>) -> Vec<ProviderEvent> {
        let mut conv = None;
        let mut usage = UsageTracker::default();
        let mut events = Vec::new();
        for json in lines {
            events.extend(parse_stream_json(json, &mut conv, &mut usage, main_model));
        }
        events
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
    fn use_tool_unwraps_to_mcp_qualified_name() {
        let mut conv = None;
        let events = parse(
            serde_json::json!({
                "type":"tool_call","toolCallId":"u1","toolName":"use_tool",
                "rawInput":{
                    "tool_name":"peckboard__search_files",
                    "tool_input":{"pattern":"foo"}
                }
            }),
            &mut conv,
        );
        let ProviderEvent::ToolStart { name, input, .. } = &events[0] else {
            panic!("expected ToolStart, got {:?}", events[0]);
        };
        assert_eq!(name, "mcp__peckboard__search_files");
        assert_eq!(input["pattern"], "foo");
    }

    #[test]
    fn use_tool_without_inner_name_stays_use_tool() {
        let mut conv = None;
        let events = parse(
            serde_json::json!({
                "type":"tool_call","toolCallId":"u2","name":"use_tool",
                "input":{"query":"whatever"}
            }),
            &mut conv,
        );
        assert!(matches!(
            &events[0],
            ProviderEvent::ToolStart { name, .. } if name == "use_tool"
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

    /// The standalone mid-stream `usage` frame is a per-response snapshot:
    /// it must not emit a `Usage` event (that would double-count against
    /// `end`), but it is recorded for occupancy.
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
    }

    /// A multi-round turn: `end.usage` sums every round; occupancy is the
    /// last per-response snapshot, not that sum. This is the grok 1.0
    /// streaming-json shape (`num_turns: 7` in the CLI docs).
    #[test]
    fn occupancy_is_last_response_not_end_sum() {
        let events = parse_turn(
            &[
                serde_json::json!({
                    "type": "usage",
                    "usage": {
                        "input_tokens": 800, "output_tokens": 40,
                        "cache_read_input_tokens": 0,
                        "cache_creation_input_tokens": 0
                    }
                }),
                serde_json::json!({
                    "type": "usage",
                    "usage": {
                        "input_tokens": 200, "output_tokens": 30,
                        "cache_read_input_tokens": 8000,
                        "cache_creation_input_tokens": 0
                    }
                }),
                serde_json::json!({
                    "type": "end", "stopReason": "end_turn", "sessionId": "s1",
                    "num_turns": 2,
                    "usage": {
                        "input_tokens": 1000, "output_tokens": 70,
                        "cache_read_input_tokens": 8000,
                        "cache_creation_input_tokens": 0,
                        "total_tokens": 9070
                    },
                    "modelUsage": {
                        "grok-4.5": {
                            "inputTokens": 1000, "outputTokens": 70,
                            "cacheReadInputTokens": 8000,
                            "cacheCreationInputTokens": 0,
                            "modelCalls": 2
                        }
                    }
                }),
            ],
            Some("grok-4.5"),
        );
        let [
            ProviderEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                total_tokens,
                context_tokens,
                model,
                ..
            },
        ] = &events[..]
        else {
            panic!("expected one Usage, got {events:?}");
        };
        // Billed slices come from the end-frame sum.
        assert_eq!(*input_tokens, 1000);
        assert_eq!(*output_tokens, 70);
        assert_eq!(*cache_read_tokens, 8000);
        assert_eq!(*total_tokens, 9070);
        // Occupancy is the last round (200 + 8000), not 1000 + 8000.
        assert_eq!(*context_tokens, 8200);
        assert_eq!(model.as_deref(), Some("grok-4.5"));
    }

    /// After a mid-turn compact the last response is smaller than earlier
    /// ones; occupancy follows the last snapshot, not the peak.
    #[test]
    fn occupancy_follows_last_response_after_compact() {
        let events = parse_turn(
            &[
                serde_json::json!({
                    "type": "usage",
                    "usage": {
                        "input_tokens": 100, "output_tokens": 10,
                        "cache_read_input_tokens": 90_000
                    }
                }),
                serde_json::json!({
                    "type": "usage",
                    "usage": {
                        "input_tokens": 80, "output_tokens": 12,
                        "cache_read_input_tokens": 5_000
                    }
                }),
                serde_json::json!({
                    "type": "end",
                    "usage": {
                        "input_tokens": 180, "output_tokens": 22,
                        "cache_read_input_tokens": 95_000
                    }
                }),
            ],
            None,
        );
        let [ProviderEvent::Usage { context_tokens, .. }] = &events[..] else {
            panic!("expected one Usage, got {events:?}");
        };
        assert_eq!(*context_tokens, 5_080);
    }

    /// A subagent modelUsage row carries billed tokens but no session
    /// occupancy — same convention as Claude.
    #[test]
    fn subagent_model_row_has_zero_context() {
        let events = parse_turn(
            &[
                serde_json::json!({
                    "type": "usage",
                    "usage": {
                        "input_tokens": 50, "output_tokens": 10,
                        "cache_read_input_tokens": 400
                    }
                }),
                serde_json::json!({
                    "type": "end",
                    "modelUsage": {
                        "grok-4.5": {
                            "inputTokens": 50, "outputTokens": 10,
                            "cacheReadInputTokens": 400
                        },
                        "grok-4-fast": {
                            "inputTokens": 20, "outputTokens": 8
                        }
                    }
                }),
            ],
            Some("grok-4.5"),
        );
        assert_eq!(events.len(), 2);
        let mut by_model: Vec<_> = events
            .iter()
            .map(|e| match e {
                ProviderEvent::Usage {
                    model,
                    context_tokens,
                    input_tokens,
                    ..
                } => (model.clone(), *context_tokens, *input_tokens),
                other => panic!("expected Usage, got {other:?}"),
            })
            .collect();
        by_model.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(by_model[0].0.as_deref(), Some("grok-4-fast"));
        assert_eq!(by_model[0].1, 0);
        assert_eq!(by_model[0].2, 20);
        assert_eq!(by_model[1].0.as_deref(), Some("grok-4.5"));
        assert_eq!(by_model[1].1, 450);
        assert_eq!(by_model[1].2, 50);
    }

    /// ACP-shaped cache keys on `modelUsage` still parse (cachedReadTokens
    /// rather than cacheReadInputTokens).
    #[test]
    fn model_usage_accepts_acp_cache_keys() {
        let events = parse(
            serde_json::json!({
                "type": "end",
                "modelUsage": {
                    "grok-4.5": {
                        "inputTokens": 10, "outputTokens": 5,
                        "cachedReadTokens": 100, "cacheCreationTokens": 20
                    }
                }
            }),
            &mut None,
        );
        let [
            ProviderEvent::Usage {
                cache_read_tokens,
                cache_creation_tokens,
                context_tokens,
                ..
            },
        ] = &events[..]
        else {
            panic!("expected one Usage, got {events:?}");
        };
        assert_eq!(*cache_read_tokens, 100);
        assert_eq!(*cache_creation_tokens, 20);
        assert_eq!(*context_tokens, 130);
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
