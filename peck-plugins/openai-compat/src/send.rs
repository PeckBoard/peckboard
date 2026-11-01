//! OpenAI Chat Completions turn (`POST {root}/v1/chat/completions`) via
//! `peckboard_http_request`.
//!
//! Non-streaming, one request per tool round. Multi-turn history lives in
//! the plugin store in OpenAI message shape (the server is stateless). When
//! `enable_tools` is on, each turn offers Peckboard MCP tools as OpenAI
//! function tools and runs calls through `peckboard_provider_invoke_mcp`.

use serde_json::{Value, json};

use crate::event::{CrashKind, ProviderEvent};
use crate::host::{self, HostFn};
use crate::models::{DEFAULT_MODEL_ID, PROVIDER_ID};
use crate::settings;

const HISTORY_COLLECTION: &str = "history";
/// Per-session `tool_use_id` allocator. Deliberately NOT the `history`
/// collection (which core wipes on session clear): persisted events outlive
/// the trimmed transcript, so ids must never repeat within a session.
const COUNTER_COLLECTION: &str = "counters";
/// Stored transcript cap. Crossing it trims down to `HISTORY_TRIM_TARGET`
/// at once, so the prompt prefix then stays byte-identical (and the
/// server's prefix cache warm) for many turns.
const MAX_HISTORY_MESSAGES: usize = 160;
const HISTORY_TRIM_TARGET: usize = MAX_HISTORY_MESSAGES * 3 / 5;
/// Max continue nudges per turn for a reply that announces a step and stops.
const MAX_CONTINUE_NUDGES: usize = 2;
const CONTINUE_NUDGE: &str =
    "Continue: make that tool call now. If you're done, give your final answer instead.";
/// No cap on tool rounds per turn: a busy model may call as many tools as
/// the task needs. A model that repeats the exact same calls this many
/// rounds in a row is stuck instead, and the turn ends there.
const MAX_IDENTICAL_TOOL_ROUNDS: usize = 3;
/// Cap on the response body quoted in an HTTP-error crash reason.
const MAX_ERROR_CHARS: usize = 2_000;

/// Per-turn token rollup. The server reports `usage` once per completion,
/// so a turn that ran tools produces one set per round; they sum into the
/// single `Usage` event the turn emits.
#[derive(Default)]
struct TurnUsage {
    /// Σ `prompt_tokens` over the turn's rounds — every prompt token the
    /// server processed, replayed history included.
    input: i64,
    /// Σ `completion_tokens` over the turn's rounds.
    output: i64,
    /// `prompt_tokens` of the last round that reported one: the context
    /// window at end of turn. Summing it would double-count the replayed
    /// transcript.
    context: i64,
}

impl TurnUsage {
    fn add_round(&mut self, prompt: i64, completion: i64) {
        self.input += prompt;
        self.output += completion;
        if prompt > 0 {
            self.context = prompt;
        }
    }

    fn is_empty(&self) -> bool {
        self.input == 0 && self.output == 0
    }
}

/// One tool call from `choices[0].message.tool_calls`, normalised: `wire_id`
/// is what the `tool` result must echo as `tool_call_id`, `args` the parsed
/// arguments object, `arguments` the JSON string sent back in history.
struct ToolCall {
    wire_id: String,
    name: String,
    args: Value,
    arguments: String,
}

pub fn run(payload: &Value) -> Result<(), String> {
    let session_id = payload
        .get("session_id")
        .and_then(|v| v.as_str())
        .ok_or("provider.send payload missing session_id")?;
    let cfg = payload.get("spawn_config").cloned().unwrap_or(json!({}));
    let message = payload.get("message").cloned().unwrap_or(json!({}));
    let prompt = message.get("text").and_then(|v| v.as_str()).unwrap_or("");
    let system_prompt = payload
        .get("system_prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let conversation_id = payload
        .get("conversation_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    // `default` (the seed placeholder, prefixed or bare) means the Default
    // Model setting.
    let Some(model_raw) = cfg
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| {
            let bare = s
                .strip_prefix(PROVIDER_ID)
                .and_then(|r| r.strip_prefix(':'));
            !s.is_empty() && *s != DEFAULT_MODEL_ID && bare != Some(DEFAULT_MODEL_ID)
        })
        .map(str::to_string)
        .or_else(|| settings::str("default_model"))
    else {
        return crash(
            session_id,
            "openai-compat plugin: no model selected — pick an openai-compat:<model> or set \
             Default Model in the provider settings",
            CrashKind::Unknown,
        );
    };
    let (model, root) = match settings::resolve_model_ref(&model_raw) {
        Ok(resolved) => resolved,
        Err(e) => return crash(session_id, &e, CrashKind::Unknown),
    };
    // Usage rows carry the fully-qualified id (the session's prefixed
    // selection).
    let prefix = format!("{PROVIDER_ID}:");
    let model_label = if model_raw.starts_with(&prefix) {
        model_raw.clone()
    } else {
        format!("{prefix}{model_raw}")
    };

    emit(
        session_id,
        &ProviderEvent::Started {
            model: model_raw.clone(),
            conversation_id: conversation_id.clone(),
            metadata: json!({ "provider": PROVIDER_ID }),
        },
    )?;

    let headers = settings::http_headers();

    // Durable history is user-first; the CURRENT system prompt is
    // prepended to the request only, never persisted.
    let mut history = load_history(session_id);
    while history
        .first()
        .and_then(|m| m.get("role").and_then(|r| r.as_str()))
        == Some("system")
    {
        history.remove(0);
    }
    history.push(user_message(prompt, &message));
    // Persist the user turn now, so an interrupt or crash mid-turn doesn't
    // forget the user said anything. Any trim happens here, before the
    // request, so this turn's prompt prefix matches what's stored.
    save_history(session_id, &mut history);

    let mut messages = request_messages(system_prompt, &history, &[]);
    // Assistant + tool turns produced this turn, appended to `history` at
    // whichever exit the turn takes (never the system message).
    let mut new_messages: Vec<Value> = Vec::new();

    let mut tools = if settings::bool("enable_tools", true) {
        function_tools(session_id)
    } else {
        None
    };
    let url = format!("{root}/v1/chat/completions");
    let mut usage = TurnUsage::default();
    let enable_thinking = settings::enable_thinking(&model);
    let (sampling, bad_sampling) = settings::sampling();
    for problem in bad_sampling {
        emit(
            session_id,
            &ProviderEvent::System {
                text: format!("Ignoring sampling setting: {problem}."),
                subtype: "warning".into(),
                detail: Value::Null,
            },
        )?;
    }

    let mut last_signature: Option<String> = None;
    let mut identical_rounds = 0;
    let mut nudges = 0;
    let mut overflow_retried = false;
    loop {
        if should_stop(session_id) {
            return interrupted_exit(session_id, &model_label, history, new_messages, &usage);
        }
        let mut body = json!({
            "model": model,
            "messages": messages,
            "stream": false,
        });
        if let Some(t) = &tools {
            body["tools"] = t.clone();
        }
        if let Some(on) = enable_thinking {
            body["chat_template_kwargs"] = json!({ "enable_thinking": on });
        }
        if let Some(obj) = body.as_object_mut() {
            obj.extend(sampling.clone());
        }
        // Blocking: `peckboard_http_request` has no cancel or poll handle
        // (one request, bounded only by `timeout_secs`), so a stop landing
        // mid-request is seen only once the server answers, and the server
        // keeps generating until then. Cancelling it would need a streaming
        // request the host can drop on stop.
        let resp = host::call_host(
            HostFn::HttpRequest,
            &json!({
                "url": url,
                "method": "POST",
                "headers": headers,
                "body": body.to_string(),
                "timeout_secs": settings::timeout_secs(),
            }),
        )?;
        let status = resp.get("status").and_then(|v| v.as_u64()).unwrap_or(0);
        let body = resp.get("body").and_then(|v| v.as_str()).unwrap_or("");
        if !(200..300).contains(&status) {
            // The prompt outgrew the model's context window: drop about half
            // of the older turns and retry once. Checked before the tools
            // retry — an overflow message can mention tools too.
            if status == 400
                && !overflow_retried
                && is_context_overflow(body)
                && trim_older_turns(&mut history)
            {
                overflow_retried = true;
                save_history(session_id, &mut history);
                messages = request_messages(system_prompt, &history, &new_messages);
                emit(
                    session_id,
                    &ProviderEvent::System {
                        text: "History trimmed to fit the model's context window; older \
                               messages were dropped."
                            .into(),
                        subtype: "warning".into(),
                        detail: Value::Null,
                    },
                )?;
                continue;
            }
            // A server/model without tool calling (e.g. vLLM without
            // --enable-auto-tool-choice) 400s a request carrying `tools`:
            // drop them for the rest of the turn and retry this round once.
            if status == 400
                && tools.is_some()
                && !is_context_overflow(body)
                && body.to_ascii_lowercase().contains("tool")
            {
                tools = None;
                emit(
                    session_id,
                    &ProviderEvent::System {
                        text: format!(
                            "Server rejected tools for model '{model}'; continuing without \
                             tools. Turn off Enable Tools to skip this retry."
                        ),
                        subtype: "warning".into(),
                        detail: Value::Null,
                    },
                )?;
                continue;
            }
            emit_usage(session_id, &model_label, &usage)?;
            return crash(
                session_id,
                &format!("openai-compat HTTP {status}: {}", error_message(body)),
                CrashKind::Unknown,
            );
        }
        // An empty or unparseable 2xx body means the server closed the
        // connection before producing anything — the one genuine NoOutput.
        let Ok(parsed) = serde_json::from_str::<Value>(body) else {
            emit_usage(session_id, &model_label, &usage)?;
            return no_output(session_id);
        };
        // Counts ride the same body; accumulate before the error check so a
        // response that also carries an error still contributes them.
        let u = parsed.get("usage");
        let count = |k: &str| u.and_then(|u| u.get(k)).and_then(|v| v.as_i64());
        usage.add_round(
            count("prompt_tokens").unwrap_or(0),
            count("completion_tokens").unwrap_or(0),
        );
        if let Some(err) = parsed.get("error").filter(|e| !e.is_null()) {
            emit_usage(session_id, &model_label, &usage)?;
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
                .or_else(|| err.as_str().map(str::to_string))
                .unwrap_or_else(|| err.to_string());
            return crash(session_id, &msg, CrashKind::Unknown);
        }
        let Some(msg) = parsed
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .cloned()
        else {
            emit_usage(session_id, &model_label, &usage)?;
            return no_output(session_id);
        };
        if let Some(think) = ["reasoning_content", "reasoning"]
            .iter()
            .find_map(|k| msg.get(*k).and_then(|v| v.as_str()))
            .filter(|s| !s.trim().is_empty())
        {
            emit(
                session_id,
                &ProviderEvent::Thinking {
                    text: think.to_string(),
                },
            )?;
        }
        // No server reasoning parser: the template's think block arrives
        // inline in `content`. Only the answer is shown and persisted.
        let (inline_think, text) =
            split_inline_think(msg.get("content").and_then(|v| v.as_str()).unwrap_or(""));
        if let Some(think) = inline_think {
            emit(session_id, &ProviderEvent::Thinking { text: think })?;
        }
        if !text.is_empty() {
            emit(session_id, &ProviderEvent::Text { text: text.clone() })?;
        }
        let finish_reason = parsed
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("finish_reason"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        if finish_reason.as_deref() == Some("length") {
            emit(
                session_id,
                &ProviderEvent::System {
                    text: "The reply hit the max-token limit and was cut off. Raise Max Tokens \
                           (or the server's context length), or send a message to continue."
                        .into(),
                    subtype: "warning".into(),
                    detail: Value::Null,
                },
            )?;
        }
        let calls = parse_tool_calls(session_id, &msg);
        let mut assistant = json!({ "role": "assistant", "content": text });
        if !calls.is_empty() {
            assistant["tool_calls"] = Value::Array(
                calls
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c.wire_id,
                            "type": "function",
                            "function": { "name": c.name, "arguments": c.arguments },
                        })
                    })
                    .collect(),
            );
        }
        messages.push(assistant.clone());
        new_messages.push(assistant);
        // The stop may have landed while the (blocking) request was in
        // flight: persist what we have instead of dropping the exchange.
        if should_stop(session_id) {
            return interrupted_exit(session_id, &model_label, history, new_messages, &usage);
        }
        if calls.is_empty() {
            // "Let me check X." and then nothing: the agent announced a
            // step but stopped. Nudge it to act, a bounded number of times;
            // the nudge is persisted so the history stays consistent.
            if tools.is_some()
                && finish_reason.as_deref() == Some("stop")
                && nudges < MAX_CONTINUE_NUDGES
                && announces_unfinished_step(&text)
            {
                nudges += 1;
                let nudge = json!({ "role": "user", "content": CONTINUE_NUDGE });
                messages.push(nudge.clone());
                new_messages.push(nudge);
                continue;
            }
            break;
        }
        let signature = tool_call_signature(&calls);
        if last_signature.as_deref() == Some(signature.as_str()) {
            identical_rounds += 1;
        } else {
            identical_rounds = 1;
            last_signature = Some(signature);
        }
        if identical_rounds >= MAX_IDENTICAL_TOOL_ROUNDS {
            // Stuck, not busy: end the turn normally without running the
            // repeat. Its calls are dropped from the stored assistant
            // message so the history has no dangling tool references.
            if let Some(obj) = new_messages.last_mut().and_then(Value::as_object_mut) {
                obj.remove("tool_calls");
            }
            emit(
                session_id,
                &ProviderEvent::System {
                    text: format!(
                        "Turn stopped: the model made the same tool call \
                         {MAX_IDENTICAL_TOOL_ROUNDS} times in a row. Send a message to continue."
                    ),
                    subtype: "warning".into(),
                    detail: Value::Null,
                },
            )?;
            break;
        }
        if run_tool_calls(session_id, &calls, &mut messages, &mut new_messages)? {
            return interrupted_exit(session_id, &model_label, history, new_messages, &usage);
        }
    }

    persist_turn(session_id, &mut history, new_messages);
    emit_usage(session_id, &model_label, &usage)?;
    emit(
        session_id,
        &ProviderEvent::Completed {
            conversation_id,
            result_meta: Value::Null,
        },
    )?;
    Ok(())
}

/// The user turn: plain string content, or OpenAI content parts
/// (`text` + `image_url` data URLs) when image attachments are present.
fn user_message(prompt: &str, message: &Value) -> Value {
    let images = image_attachments(message);
    if images.is_empty() {
        return json!({ "role": "user", "content": prompt });
    }
    let mut parts = vec![json!({ "type": "text", "text": prompt })];
    for (mime, data) in images {
        parts.push(json!({
            "type": "image_url",
            "image_url": { "url": format!("data:{mime};base64,{data}") },
        }));
    }
    json!({ "role": "user", "content": parts })
}

/// `(thinking, answer)` from a `content` the chat template's think block
/// leaked into (no server reasoning parser): `reasoning</think>answer` —
/// the opening `<think>` usually sits in the prompt — or
/// `<think>reasoning</think>answer`. Splits on the LAST `</think>`; with
/// none, the content is all answer.
fn split_inline_think(content: &str) -> (Option<String>, String) {
    const CLOSE: &str = "</think>";
    let Some(idx) = content.rfind(CLOSE) else {
        return (None, content.to_string());
    };
    let before = content[..idx].trim_start();
    let think = before.strip_prefix("<think>").unwrap_or(before).trim();
    let answer = content[idx + CLOSE.len()..].trim_start().to_string();
    ((!think.is_empty()).then(|| think.to_string()), answer)
}

/// Whether a reply's last sentence announces a step it never took
/// ("Let me check the logs.", "Three questions. Let me work through
/// them.", "Now I'll run the tests:") — the agent stopping mid-task instead
/// of making the tool call.
fn announces_unfinished_step(answer: &str) -> bool {
    let Some(line) = answer.lines().map(str::trim).rfind(|l| !l.is_empty()) else {
        return false;
    };
    if line.ends_with(':') {
        return true;
    }
    // Last sentence of the last line: a one-line reply often leads with a
    // recap ("Three questions, all read-only.") before the announcement.
    let body = line.trim_end_matches(|c: char| matches!(c, '.' | '!' | '…' | ' '));
    let sentence = body.rfind(['.', '!', '?']).map_or(body, |i| &body[i + 1..]);
    let lower = sentence.to_lowercase().replace('\u{2019}', "'");
    let mut lower = lower.trim_start_matches(|c: char| !c.is_alphanumeric());
    // Discourse openers in front of the announcement ("Now, let me …").
    for opener in [
        "now", "next", "first", "then", "ok", "okay", "alright", "good", "great",
    ] {
        if let Some(rest) = lower.strip_prefix(opener)
            && rest.starts_with([',', ' '])
        {
            lower = rest.trim_start_matches([',', ' ']);
            break;
        }
    }
    if lower.starts_with("let me know") {
        return false;
    }
    [
        "let me ",
        "i'll ",
        "i will ",
        "let's ",
        "i am going to ",
        "i'm going to ",
    ]
    .iter()
    .any(|p| lower.starts_with(p))
}

/// Best human-readable message from an error body: OpenAI's
/// `{"error":{"message"}}`, a bare `{"error":"…"}` / `{"message"}` /
/// `{"detail"}`, else the raw (truncated) body.
fn error_message(body: &str) -> String {
    let from_json = serde_json::from_str::<Value>(body).ok().and_then(|v| {
        let e = v.get("error");
        e.and_then(|e| e.get("message"))
            .or(e)
            .or_else(|| v.get("message"))
            .or_else(|| v.get("detail"))
            .and_then(|m| m.as_str())
            .map(str::to_string)
    });
    let msg = from_json.unwrap_or_else(|| body.to_string());
    msg.chars().take(MAX_ERROR_CHARS).collect()
}

/// Normalise `message.tool_calls`. Arguments arrive as a JSON string
/// (sometimes an object on lax servers); unparseable or empty arguments
/// become `{}` rather than failing the turn. A call without an id gets a
/// generated one so the `tool` result can still reference it.
fn parse_tool_calls(session_id: &str, msg: &Value) -> Vec<ToolCall> {
    let Some(arr) = msg.get("tool_calls").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for call in arr {
        let fn_obj = call.get("function").unwrap_or(call);
        let name = fn_obj
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        let args = match fn_obj.get("arguments") {
            Some(Value::String(s)) if !s.trim().is_empty() => {
                serde_json::from_str::<Value>(s).unwrap_or_else(|_| json!({}))
            }
            Some(v @ Value::Object(_)) => v.clone(),
            _ => json!({}),
        };
        let args = if args.is_object() { args } else { json!({}) };
        let wire_id = call
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                format!("call_{}", alloc_tool_ids(&format!("{session_id}:wire"), 1))
            });
        out.push(ToolCall {
            wire_id,
            name,
            arguments: args.to_string(),
            args,
        });
    }
    out
}

/// What a round asked for — each call's name + canonical arguments, ignoring
/// the per-round wire ids — so two rounds compare equal exactly when the
/// model repeated itself.
fn tool_call_signature(calls: &[ToolCall]) -> String {
    calls
        .iter()
        .map(|c| format!("{}\u{0}{}", c.name, c.arguments))
        .collect::<Vec<_>>()
        .join("\u{1}")
}
/// Emit the turn's single `Usage` event, or nothing when the server
/// reported no counts.
fn emit_usage(session_id: &str, model_label: &str, usage: &TurnUsage) -> Result<(), String> {
    if usage.is_empty() {
        return Ok(());
    }
    emit(
        session_id,
        &ProviderEvent::Usage {
            input_tokens: usage.input,
            output_tokens: usage.output,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            total_tokens: usage.context + usage.output,
            context_tokens: usage.context,
            model: Some(model_label.to_string()),
            turn_seq: None,
        },
    )
}

/// The user stopped the turn: emit accumulated usage, mark the partial
/// exchange with the interrupt marker, persist it (instead of forgetting
/// the turn happened), and report `Crashed{Interrupted}`.
fn interrupted_exit(
    session_id: &str,
    model_label: &str,
    mut history: Vec<Value>,
    mut new_messages: Vec<Value>,
    usage: &TurnUsage,
) -> Result<(), String> {
    emit_usage(session_id, model_label, usage)?;
    // A continue nudge the model never answered would leave two user turns
    // in a row once the next message arrives.
    if new_messages
        .last()
        .and_then(|m| m.get("role").and_then(|r| r.as_str()))
        == Some("user")
    {
        new_messages.pop();
    }
    if let Some(last) = new_messages.last_mut()
        && last.get("role").and_then(|r| r.as_str()) == Some("assistant")
        && let Some(text) = last
            .get("content")
            .and_then(|c| c.as_str())
            .filter(|s| !s.is_empty())
    {
        last["content"] = json!(format!("{text}\n\n[Response interrupted by user]"));
    }
    // An assistant turn whose tool calls never got results is a dangling
    // reference servers reject on the next request: drop the unanswered
    // calls from the tail.
    drop_unanswered_tool_calls(&mut new_messages);
    history.extend(new_messages);
    save_history(session_id, &mut history);
    crash(session_id, "interrupted", CrashKind::Interrupted)
}

/// Strip `tool_calls` entries with no matching `tool` result from the last
/// assistant message (an interrupt between calls leaves some unanswered).
fn drop_unanswered_tool_calls(messages: &mut [Value]) {
    let answered: Vec<String> = messages
        .iter()
        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
        .filter_map(|m| {
            m.get("tool_call_id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect();
    let Some(last_assistant) = messages
        .iter_mut()
        .rev()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"))
    else {
        return;
    };
    let Some(calls) = last_assistant
        .get_mut("tool_calls")
        .and_then(|v| v.as_array_mut())
    else {
        return;
    };
    calls.retain(|c| {
        c.get("id")
            .and_then(|v| v.as_str())
            .is_some_and(|id| answered.iter().any(|a| a == id))
    });
    if calls.is_empty()
        && let Some(obj) = last_assistant.as_object_mut()
    {
        obj.remove("tool_calls");
    }
}

/// Append this turn's generated messages to the durable transcript.
fn persist_turn(session_id: &str, history: &mut Vec<Value>, new_messages: Vec<Value>) {
    history.extend(new_messages);
    save_history(session_id, history);
}

/// Tool Set "Core coding tools": names offered on top of Extra Tools. Only
/// those the session actually exposes are sent (e.g. `graphify_*` exist only
/// while that plugin is active).
const CORE_TOOLS: &[&str] = &[
    "ask_user",
    "background_status",
    "browser_act",
    "browser_find",
    "browser_open",
    "browser_outline",
    "browser_screenshot",
    "edit_file",
    "fetch_url",
    "file_outline",
    "finish_card",
    "git",
    "graphify_explain",
    "graphify_path",
    "list_files",
    "move_card_to_done",
    "read_file",
    "read_symbol",
    "run_background",
    "run_command",
    "run_tests",
    "search_files",
    "search_web",
    "update_card",
    "write_file",
];

/// Peckboard MCP tools as OpenAI `{"type":"function","function":{…}}` defs,
/// narrowed by the Tool Set setting and sorted by name. ~200 definitions
/// cost ~45k prompt tokens and make a small model pick tools badly; the
/// sort keeps the array byte-identical across requests for the prefix cache.
fn function_tools(session_id: &str) -> Option<Value> {
    let mcp = host::call_host(
        HostFn::ProviderGetMcpConfig,
        &json!({ "session_id": session_id }),
    )
    .ok()?;
    let allowed = settings::str("tool_set")
        .filter(|s| s == "core")
        .map(|_| settings::str_list("extra_tools"));
    let offered = |name: &str| match &allowed {
        None => true,
        Some(extra) => CORE_TOOLS.contains(&name) || extra.iter().any(|e| e == name),
    };
    let defs = mcp.get("tool_defs")?.as_array()?;
    let mut tools: Vec<(String, Value)> = defs
        .iter()
        .filter_map(|d| {
            let name = d.get("name")?.as_str()?.to_string();
            if name.is_empty() || !offered(&name) {
                return None;
            }
            let def = json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": d.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                    "parameters": d.get("input_schema").cloned().unwrap_or(json!({"type": "object"})),
                }
            });
            Some((name, def))
        })
        .collect();
    tools.sort_by(|a, b| a.0.cmp(&b.0));
    if tools.is_empty() {
        None
    } else {
        Some(Value::Array(tools.into_iter().map(|(_, d)| d).collect()))
    }
}

/// Reserve `count` consecutive ordinals for `key`, bumping the durable
/// counter BEFORE any id is emitted so a crash mid-round can never hand the
/// same id out twice.
fn alloc_tool_ids(key: &str, count: u64) -> u64 {
    let cur = host::call_host(
        HostFn::StoreGet,
        &json!({ "collection": COUNTER_COLLECTION, "key": key }),
    )
    .ok()
    .and_then(|v| v.get("value").and_then(|x| x.as_u64()))
    .unwrap_or(0);
    let _ = host::call_host(
        HostFn::StorePut,
        &json!({
            "collection": COUNTER_COLLECTION,
            "key": key,
            "data": cur + count,
        }),
    );
    cur
}

/// Run one round's tool calls, feeding each result back as a `tool` turn.
/// Returns `Ok(true)` when the user interrupted between calls — the caller
/// persists the partial exchange.
fn run_tool_calls(
    session_id: &str,
    calls: &[ToolCall],
    messages: &mut Vec<Value>,
    new_messages: &mut Vec<Value>,
) -> Result<bool, String> {
    let base = alloc_tool_ids(session_id, calls.len() as u64);
    for (i, call) in calls.iter().enumerate() {
        if should_stop(session_id) {
            return Ok(true);
        }
        let tool_id = format!("{PROVIDER_ID}-tool-{}", base + i as u64);
        emit(
            session_id,
            &ProviderEvent::ToolStart {
                tool_use_id: tool_id.clone(),
                name: call.name.clone(),
                input: call.args.clone(),
            },
        )?;
        let invoked = host::call_host(
            HostFn::ProviderInvokeMcp,
            &json!({
                "session_id": session_id,
                "name": call.name,
                "arguments": call.args,
            }),
        );
        let (content, ok) = match invoked {
            Ok(v) if v.get("ok").and_then(|o| o.as_bool()) == Some(true) => {
                let result = v.get("result").cloned().unwrap_or(Value::Null);
                (tool_result_text(&result), true)
            }
            Ok(v) => (
                v.get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("tool failed")
                    .to_string(),
                false,
            ),
            Err(e) => (e, false),
        };
        emit(
            session_id,
            &ProviderEvent::ToolEnd {
                tool_use_id: tool_id,
                output: if ok { Some(content.clone()) } else { None },
                error: if ok { None } else { Some(content.clone()) },
                images: Vec::new(),
            },
        )?;
        // The UI keeps the full output; the model gets a capped copy so one
        // huge result can't blow the context window for every later round.
        let tool_msg = json!({
            "role": "tool",
            "tool_call_id": call.wire_id,
            "content": cap_tool_result(&content, settings::max_tool_result_chars()),
        });
        messages.push(tool_msg.clone());
        new_messages.push(tool_msg);
    }
    Ok(false)
}

/// A tool result as the plain text the model reads: a string as-is, an
/// MCP-shaped `{"content":[{"type":"text","text":…}]}` as its text parts
/// joined, anything else (core tools return JSON objects) as pretty JSON.
/// `Value::to_string` would hand the model a JSON-quoted string with
/// escaped newlines.
fn tool_result_text(result: &Value) -> String {
    if let Value::String(s) = result {
        return s.clone();
    }
    if let Some(parts) = result.get("content").and_then(|c| c.as_array()) {
        let texts: Vec<&str> = parts
            .iter()
            .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect();
        if !texts.is_empty() {
            return texts.join("\n");
        }
    }
    serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string())
}

/// `text` cut to about `max` chars — the head and the tail, with a marker
/// in the middle saying how much was dropped and how to see it. Unchanged
/// when it fits or there's no cap.
fn cap_tool_result(text: &str, max: Option<usize>) -> String {
    let Some(max) = max else {
        return text.to_string();
    };
    let total = text.chars().count();
    if total <= max {
        return text.to_string();
    }
    let head = max / 2;
    let tail = max - head;
    let omitted = total - head - tail;
    let byte_at = |n: usize| text.char_indices().nth(n).map_or(text.len(), |(i, _)| i);
    format!(
        "{}\n\n[… {} chars omitted: narrow the request (line ranges, filters, tail) to see more …]\n\n{}",
        &text[..byte_at(head)],
        thousands(omitted),
        &text[byte_at(total - tail)..],
    )
}

/// `183402` → `183,402`.
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn load_history(session_id: &str) -> Vec<Value> {
    let v = host::call_host(
        HostFn::StoreGet,
        &json!({ "collection": HISTORY_COLLECTION, "key": session_id }),
    )
    .ok();
    v.and_then(|v| v.get("value").cloned())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

/// The request transcript: the CURRENT system prompt (never persisted),
/// the stored history, then this turn's generated messages.
fn request_messages(system_prompt: &str, history: &[Value], new_messages: &[Value]) -> Vec<Value> {
    let mut messages = Vec::with_capacity(history.len() + new_messages.len() + 1);
    if !system_prompt.trim().is_empty() {
        messages.push(json!({ "role": "system", "content": system_prompt }));
    }
    messages.extend_from_slice(history);
    messages.extend_from_slice(new_messages);
    messages
}

/// Whether an error body is the server saying the prompt exceeds the
/// model's context window (vLLM, OpenAI, llama.cpp, Anthropic-style
/// proxies all word it differently).
fn is_context_overflow(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    [
        "maximum context length",
        "context_length_exceeded",
        "prompt is too long",
        "reduce the length",
    ]
    .iter()
    .any(|p| b.contains(p))
}

/// Drop about half of the turns before the current user message (always
/// the last entry), realigned to a user-message boundary. `false` when
/// there was nothing older to drop.
fn trim_older_turns(history: &mut Vec<Value>) -> bool {
    let older = history.len().saturating_sub(1);
    if older == 0 {
        return false;
    }
    history.drain(0..older.div_ceil(2));
    while history.len() > 1
        && history
            .first()
            .and_then(|m| m.get("role").and_then(|r| r.as_str()))
            != Some("user")
    {
        history.remove(0);
    }
    true
}

/// Store the transcript, trimming it in place first so the caller keeps
/// exactly what was stored. Over `MAX_HISTORY_MESSAGES` it drops down to
/// `HISTORY_TRIM_TARGET` in one go (hysteresis), not just the overflow:
/// trimming one message per turn would change the prompt prefix every turn
/// and defeat the server's prefix cache, re-prefilling the whole context.
fn save_history(session_id: &str, messages: &mut Vec<Value>) {
    if messages.len() > MAX_HISTORY_MESSAGES {
        let drop = messages.len() - HISTORY_TRIM_TARGET;
        messages.drain(0..drop);
        // Align to a user-message boundary: a leading `tool` result (or an
        // assistant turn whose `tool_calls` were trimmed off the front) is
        // a dangling reference servers reject.
        while matches!(
            messages
                .first()
                .and_then(|m| m.get("role").and_then(|r| r.as_str())),
            Some("assistant") | Some("tool")
        ) {
            messages.remove(0);
        }
    }
    let _ = host::call_host(
        HostFn::StorePut,
        &json!({
            "collection": HISTORY_COLLECTION,
            "key": session_id,
            "data": messages,
        }),
    );
}

/// `(mime, base64)` for every image attachment on the message.
fn image_attachments(message: &Value) -> Vec<(String, String)> {
    let Some(atts) = message.get("attachments").and_then(|a| a.as_array()) else {
        return Vec::new();
    };
    atts.iter()
        .filter_map(|a| {
            let mime = a.get("mime_type")?.as_str()?;
            if !mime.starts_with("image/") {
                return None;
            }
            let data = a.get("data_base64")?.as_str()?;
            Some((mime.to_string(), data.to_string()))
        })
        .collect()
}

fn no_output(session_id: &str) -> Result<(), String> {
    crash(
        session_id,
        "OpenAI-compatible server closed the stream before producing output",
        CrashKind::NoOutput,
    )
}

fn crash(session_id: &str, reason: &str, kind: CrashKind) -> Result<(), String> {
    emit(
        session_id,
        &ProviderEvent::Crashed {
            reason: reason.into(),
            error_kind: kind,
            exit_code: None,
            stderr: None,
        },
    )
}

fn emit(session_id: &str, event: &ProviderEvent) -> Result<(), String> {
    let value = serde_json::to_value(event).map_err(|e| e.to_string())?;
    host::call_host(
        HostFn::EmitProviderEvent,
        &json!({ "session_id": session_id, "event": value }),
    )
    .map(|_| ())
}

fn should_stop(session_id: &str) -> bool {
    host::call_host(
        HostFn::ProviderShouldStop,
        &json!({ "session_id": session_id }),
    )
    .ok()
    .and_then(|v| v.get("stop").and_then(|s| s.as_bool()))
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// In-memory host double answering the subset of host functions a turn
    /// touches. Chat responses are served in order; the last one repeats.
    /// `stop_from` = the 1-based `should_stop` call number from which the
    /// answer flips to `true` (0 = never).
    struct FakeHost {
        store: Arc<Mutex<HashMap<(String, String), Value>>>,
        events: Arc<Mutex<Vec<Value>>>,
        chat_bodies: Arc<Mutex<Vec<Value>>>,
        chat_urls: Arc<Mutex<Vec<String>>>,
        invoked: Arc<Mutex<Vec<Value>>>,
        chat_responses: Vec<Value>,
        stop_from: usize,
        stop_calls: Arc<Mutex<usize>>,
        tool_defs: Option<Value>,
        /// Plugin settings by key; unset keys answer `null`.
        settings: HashMap<String, Value>,
        /// What every `invoke_mcp` call returns as `result`.
        tool_result: Value,
    }

    impl FakeHost {
        fn new(chat_responses: Vec<Value>) -> Self {
            FakeHost {
                store: Arc::default(),
                events: Arc::default(),
                chat_bodies: Arc::default(),
                chat_urls: Arc::default(),
                invoked: Arc::default(),
                chat_responses,
                stop_from: 0,
                stop_calls: Arc::default(),
                tool_defs: None,
                settings: HashMap::new(),
                tool_result: json!({ "files": ["a.rs"] }),
            }
        }

        fn host_fn(&self) -> peck_plugin_native_host::HostFn {
            let store = self.store.clone();
            let events = self.events.clone();
            let chat_bodies = self.chat_bodies.clone();
            let chat_urls = self.chat_urls.clone();
            let invoked = self.invoked.clone();
            let chat_responses = self.chat_responses.clone();
            let stop_from = self.stop_from;
            let stop_calls = self.stop_calls.clone();
            let tool_defs = self.tool_defs.clone();
            let settings = self.settings.clone();
            let tool_result = self.tool_result.clone();
            Arc::new(move |name: &str, input: &str| {
                let input: Value = serde_json::from_str(input).unwrap_or(json!({}));
                let key = |v: &Value| {
                    (
                        v.get("collection")
                            .and_then(|c| c.as_str())
                            .unwrap_or("")
                            .to_string(),
                        v.get("key")
                            .and_then(|k| k.as_str())
                            .unwrap_or("")
                            .to_string(),
                    )
                };
                match name {
                    "peckboard_get_plugin_setting" => {
                        let k = input.get("key").and_then(|k| k.as_str()).unwrap_or("");
                        json!({ "value": settings.get(k) }).to_string()
                    }
                    "peckboard_store_get" => {
                        let v = store.lock().unwrap().get(&key(&input)).cloned();
                        json!({ "value": v }).to_string()
                    }
                    "peckboard_store_put" => {
                        let data = input.get("data").cloned().unwrap_or(Value::Null);
                        store.lock().unwrap().insert(key(&input), data);
                        json!({ "ok": true }).to_string()
                    }
                    "peckboard_emit_provider_event" => {
                        events
                            .lock()
                            .unwrap()
                            .push(input.get("event").cloned().unwrap_or(Value::Null));
                        json!({ "ok": true }).to_string()
                    }
                    "peckboard_provider_should_stop" => {
                        let mut n = stop_calls.lock().unwrap();
                        *n += 1;
                        json!({ "stop": stop_from != 0 && *n >= stop_from }).to_string()
                    }
                    "peckboard_provider_get_mcp_config" => match &tool_defs {
                        Some(defs) => json!({ "tool_defs": defs }).to_string(),
                        None => json!({ "error": "no mcp in test" }).to_string(),
                    },
                    "peckboard_provider_invoke_mcp" => {
                        invoked.lock().unwrap().push(input.clone());
                        json!({ "ok": true, "result": tool_result }).to_string()
                    }
                    "peckboard_http_request" => {
                        let url = input.get("url").and_then(|u| u.as_str()).unwrap_or("");
                        chat_urls.lock().unwrap().push(url.to_string());
                        let body = input.get("body").and_then(|b| b.as_str()).unwrap_or("{}");
                        let mut bodies = chat_bodies.lock().unwrap();
                        bodies.push(serde_json::from_str(body).unwrap_or(json!({})));
                        let i = (bodies.len() - 1).min(chat_responses.len() - 1);
                        chat_responses[i].to_string()
                    }
                    other => json!({ "error": format!("unexpected host fn {other}") }).to_string(),
                }
            })
        }

        fn saved_history(&self) -> Vec<Value> {
            self.store
                .lock()
                .unwrap()
                .get(&(HISTORY_COLLECTION.to_string(), "s1".to_string()))
                .unwrap()
                .as_array()
                .unwrap()
                .clone()
        }
    }

    fn completion(message: Value, prompt: i64, completion: i64) -> Value {
        finished(message, prompt, completion, "stop")
    }

    fn finished(message: Value, prompt: i64, completion: i64, finish_reason: &str) -> Value {
        json!({
            "status": 200,
            "body": json!({
                "choices": [{ "index": 0, "message": message, "finish_reason": finish_reason }],
                "usage": { "prompt_tokens": prompt, "completion_tokens": completion },
            })
            .to_string(),
        })
    }

    fn payload() -> Value {
        json!({
            "session_id": "s1",
            "spawn_config": { "model": "openai-compat:Qwen/Qwen3-32B" },
            "message": { "text": "hello" },
            "system_prompt": "NEW PROMPT",
        })
    }

    #[test]
    fn turn_usage_sums_rounds_and_keeps_last_context() {
        let mut usage = TurnUsage::default();
        assert!(usage.is_empty());
        usage.add_round(100, 20);
        usage.add_round(240, 15);
        usage.add_round(0, 0);
        usage.add_round(500, 30);
        assert_eq!(usage.input, 840);
        assert_eq!(usage.output, 65);
        assert_eq!(usage.context, 500);
    }

    /// The system prompt is prepended FRESH each turn and never persisted:
    /// a legacy stored system message is stripped, the current prompt leads
    /// the outgoing request, and the saved transcript stays user-first.
    #[test]
    fn system_prompt_is_fresh_per_turn_and_never_persisted() {
        let host = FakeHost::new(vec![completion(
            json!({ "role": "assistant", "content": "answer", "reasoning_content": "hmm" }),
            50,
            7,
        )]);
        host.store.lock().unwrap().insert(
            (HISTORY_COLLECTION.to_string(), "s1".to_string()),
            json!([
                { "role": "system", "content": "OLD PROMPT" },
                { "role": "user", "content": "q1" },
                { "role": "assistant", "content": "a1" },
            ]),
        );
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();

        let urls = host.chat_urls.lock().unwrap();
        assert_eq!(urls[0], "http://localhost:8000/v1/chat/completions");
        let bodies = host.chat_bodies.lock().unwrap();
        assert_eq!(bodies[0]["model"], "Qwen/Qwen3-32B");
        assert_eq!(bodies[0]["stream"], false);
        let msgs = bodies[0]["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "NEW PROMPT");
        assert_eq!(msgs[1]["content"], "q1");
        assert!(!bodies[0].to_string().contains("OLD PROMPT"));

        let saved = host.saved_history();
        assert_eq!(saved.first().unwrap()["role"], "user");
        assert!(!saved.iter().any(|m| m["role"] == "system"));
        assert_eq!(saved.last().unwrap()["content"], "answer");

        let events = host.events.lock().unwrap();
        let thinking = events.iter().find(|e| e["kind"] == "thinking").unwrap();
        assert_eq!(thinking["text"], "hmm");
        let usage = events.iter().find(|e| e["kind"] == "usage").unwrap();
        assert_eq!(usage["model"], "openai-compat:Qwen/Qwen3-32B");
        assert_eq!(usage["context_tokens"], 50);
        assert_eq!(usage["total_tokens"], 57);
        assert!(events.iter().any(|e| e["kind"] == "completed"));
    }

    /// An empty/unparseable 2xx body is a NoOutput crash, not a silent
    /// completion with an empty message.
    #[test]
    fn empty_response_crashes_with_no_output() {
        let host = FakeHost::new(vec![json!({ "status": 200, "body": "" })]);
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        let events = host.events.lock().unwrap();
        let crashed = events.iter().find(|e| e["kind"] == "crashed").unwrap();
        assert_eq!(crashed["error_kind"], "no_output");
        assert!(!events.iter().any(|e| e["kind"] == "completed"));
    }
    /// The `default` seed placeholder with no Default Model configured is a
    /// clear crash, never a request for a model literally named `default`.
    #[test]
    fn default_placeholder_without_setting_crashes_clearly() {
        let host = FakeHost::new(vec![json!({ "status": 200, "body": "" })]);
        let mut p = payload();
        p["spawn_config"]["model"] = json!("openai-compat:default");
        peck_plugin_native_host::with_host(host.host_fn(), || run(&p)).unwrap();
        assert!(host.chat_bodies.lock().unwrap().is_empty());
        let events = host.events.lock().unwrap();
        let crashed = events.iter().find(|e| e["kind"] == "crashed").unwrap();
        assert!(
            crashed["reason"]
                .as_str()
                .unwrap()
                .contains("no model selected")
        );
    }

    /// A non-2xx answer surfaces status + the server's error message.
    #[test]
    fn http_error_reports_status_and_message() {
        let host = FakeHost::new(vec![json!({
            "status": 404,
            "body": json!({ "error": { "message": "model 'x' not found" } }).to_string(),
        })]);
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        let events = host.events.lock().unwrap();
        let crashed = events.iter().find(|e| e["kind"] == "crashed").unwrap();
        assert_eq!(
            crashed["reason"],
            "openai-compat HTTP 404: model 'x' not found"
        );
    }

    /// A stop landing while the request was in flight persists the partial
    /// exchange with the interrupt marker and emits the accumulated usage.
    #[test]
    fn interrupt_persists_marked_partial_and_usage() {
        let mut host = FakeHost::new(vec![completion(
            json!({ "role": "assistant", "content": "half an answer" }),
            11,
            4,
        )]);
        // First should_stop (loop top) passes; second (post-response) stops.
        host.stop_from = 2;
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();

        let events = host.events.lock().unwrap();
        let crashed = events.iter().find(|e| e["kind"] == "crashed").unwrap();
        assert_eq!(crashed["error_kind"], "interrupted");
        let usage = events.iter().find(|e| e["kind"] == "usage").unwrap();
        assert_eq!(usage["total_tokens"], 15);

        let saved = host.saved_history();
        let last = saved.last().unwrap();
        assert_eq!(last["role"], "assistant");
        assert_eq!(
            last["content"],
            "half an answer\n\n[Response interrupted by user]"
        );
    }

    /// tool_use_ids come from a durable per-session counter, so they never
    /// repeat across rounds or turns.
    #[test]
    fn tool_ids_are_unique_across_rounds_and_turns() {
        let host = FakeHost::new(vec![json!({ "status": 200, "body": "" })]);
        let (a, b) = peck_plugin_native_host::with_host(host.host_fn(), || {
            (alloc_tool_ids("s1", 3), alloc_tool_ids("s1", 2))
        });
        assert_eq!(a, 0);
        assert_eq!(b, 3);
    }

    /// One tool round in OpenAI shape: tools offered as functions, the
    /// string `arguments` parsed for the MCP call, and the follow-up request
    /// carries the assistant `tool_calls` plus a `tool` result echoing the
    /// server's call id.
    #[test]
    fn tool_call_round_translates_to_openai_messages() {
        let mut host = FakeHost::new(vec![
            completion(
                json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": { "name": "list_files", "arguments": "{\"path\":\"src\"}" },
                    }],
                }),
                30,
                5,
            ),
            completion(json!({ "role": "assistant", "content": "done" }), 60, 3),
        ]);
        host.tool_defs = Some(json!([{
            "name": "list_files",
            "description": "List files",
            "input_schema": { "type": "object", "properties": { "path": { "type": "string" } } },
        }]));
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();

        let bodies = host.chat_bodies.lock().unwrap();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0]["tools"][0]["type"], "function");
        assert_eq!(bodies[0]["tools"][0]["function"]["name"], "list_files");

        let invoked = host.invoked.lock().unwrap();
        assert_eq!(invoked[0]["name"], "list_files");
        assert_eq!(invoked[0]["arguments"], json!({ "path": "src" }));

        let msgs = bodies[1]["messages"].as_array().unwrap();
        let assistant = &msgs[msgs.len() - 2];
        assert_eq!(assistant["role"], "assistant");
        let call = &assistant["tool_calls"][0];
        assert_eq!(call["id"], "call_abc");
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "list_files");
        let args: Value =
            serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args, json!({ "path": "src" }));
        let tool = &msgs[msgs.len() - 1];
        assert_eq!(tool["role"], "tool");
        assert_eq!(tool["tool_call_id"], "call_abc");
        assert_eq!(tool["content"], "{\n  \"files\": [\n    \"a.rs\"\n  ]\n}");

        let events = host.events.lock().unwrap();
        let start = events.iter().find(|e| e["kind"] == "tool_start").unwrap();
        assert_eq!(start["tool_use_id"], "openai-compat-tool-0");
        assert_eq!(start["input"], json!({ "path": "src" }));
        let usage = events.iter().find(|e| e["kind"] == "usage").unwrap();
        assert_eq!(usage["input_tokens"], 90);
        assert_eq!(usage["context_tokens"], 60);
        assert!(events.iter().any(|e| e["kind"] == "completed"));
    }

    /// A 400 that blames tools drops them and retries the round once.
    #[test]
    fn tools_rejected_retries_without_tools() {
        let mut host = FakeHost::new(vec![
            json!({
                "status": 400,
                "body": json!({ "error": { "message": "\"auto\" tool choice requires --enable-auto-tool-choice" } }).to_string(),
            }),
            completion(json!({ "role": "assistant", "content": "ok" }), 5, 1),
        ]);
        host.tool_defs = Some(json!([{ "name": "t", "input_schema": { "type": "object" } }]));
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        let bodies = host.chat_bodies.lock().unwrap();
        assert_eq!(bodies.len(), 2);
        assert!(bodies[0].get("tools").is_some());
        assert!(bodies[1].get("tools").is_none());
        let events = host.events.lock().unwrap();
        assert!(events.iter().any(|e| e["kind"] == "completed"));
    }

    /// A model repeating the exact same call is stopped after
    /// `MAX_IDENTICAL_TOOL_ROUNDS` rounds: the repeat isn't run, the turn
    /// completes (not crashes) with a warning, and the stored history ends
    /// on an assistant message with no dangling `tool_calls`.
    #[test]
    fn repeated_identical_tool_calls_end_the_turn() {
        let same_call = |id: &str| {
            completion(
                json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": id,
                        "type": "function",
                        "function": { "name": "t", "arguments": "{\"a\":1}" },
                    }],
                }),
                10,
                1,
            )
        };
        let mut host = FakeHost::new(vec![same_call("c1"), same_call("c2"), same_call("c3")]);
        host.tool_defs = Some(json!([{ "name": "t", "input_schema": { "type": "object" } }]));
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();

        assert_eq!(host.chat_bodies.lock().unwrap().len(), 3);
        assert_eq!(host.invoked.lock().unwrap().len(), 2);
        let events = host.events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|e| e["kind"] == "system" && e["subtype"] == "warning")
        );
        assert!(events.iter().any(|e| e["kind"] == "completed"));
        assert!(!events.iter().any(|e| e["kind"] == "crashed"));
        drop(events);
        let saved = host.saved_history();
        let last = saved.last().unwrap();
        assert_eq!(last["role"], "assistant");
        assert!(last.get("tool_calls").is_none());
    }

    #[test]
    fn image_attachments_become_data_url_parts() {
        let m = user_message(
            "look",
            &json!({ "attachments": [
                { "mime_type": "image/png", "data_base64": "AAAA" },
                { "mime_type": "text/plain", "data_base64": "BBBB" },
            ] }),
        );
        let parts = m["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], json!({ "type": "text", "text": "look" }));
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,AAAA");
    }

    fn answer(content: &str) -> Value {
        completion(json!({ "role": "assistant", "content": content }), 10, 2)
    }

    /// Thinking: Auto sends `enable_thinking: true` for Qwen3 ids only, On
    /// and Off force it either way.
    #[test]
    fn thinking_kwarg_follows_mode_and_model() {
        for (mode, model, want) in [
            (None, "openai-compat:Qwen/Qwen3-32B", Some(true)),
            (Some("auto"), "openai-compat:llama-3.1-8b", None),
            (Some("on"), "openai-compat:llama-3.1-8b", Some(true)),
            (Some("off"), "openai-compat:Qwen/Qwen3-32B", Some(false)),
        ] {
            let mut host = FakeHost::new(vec![answer("ok")]);
            if let Some(m) = mode {
                host.settings.insert("thinking".into(), json!(m));
            }
            let mut p = payload();
            p["spawn_config"]["model"] = json!(model);
            peck_plugin_native_host::with_host(host.host_fn(), || run(&p)).unwrap();
            let body = &host.chat_bodies.lock().unwrap()[0];
            let got = body
                .get("chat_template_kwargs")
                .map(|k| k["enable_thinking"].as_bool().unwrap());
            assert_eq!(got, want, "mode {mode:?}, model {model}");
            assert!(body.get("temperature").is_none() && body.get("max_tokens").is_none());
        }
    }

    /// Inline `reasoning</think>answer` content: the reasoning becomes a
    /// thinking event and only the answer is shown and persisted.
    #[test]
    fn inline_think_is_split_and_only_the_answer_persisted() {
        let host = FakeHost::new(vec![answer("weigh options\n</think>\n\nThe answer.")]);
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        let events = host.events.lock().unwrap();
        let thinking = events.iter().find(|e| e["kind"] == "thinking").unwrap();
        assert_eq!(thinking["text"], "weigh options");
        let text = events.iter().find(|e| e["kind"] == "text").unwrap();
        assert_eq!(text["text"], "The answer.");
        drop(events);
        assert_eq!(
            host.saved_history().last().unwrap()["content"],
            "The answer."
        );

        assert_eq!(
            split_inline_think("<think>a</think>b"),
            (Some("a".into()), "b".into())
        );
        assert_eq!(
            split_inline_think("x</think>y</think>z"),
            (Some("x</think>y".into()), "z".into())
        );
        assert_eq!(split_inline_think("plain"), (None, "plain".into()));
        assert_eq!(split_inline_think("<think></think>hi"), (None, "hi".into()));
    }

    /// Sampling settings ride the request only when set; an out-of-range
    /// float is dropped with a warning instead of 400ing every turn.
    #[test]
    fn sampling_settings_pass_through_when_set() {
        let mut host = FakeHost::new(vec![answer("ok")]);
        for (k, v) in [
            ("temperature", json!("5")),
            ("top_p", json!("0.95")),
            ("top_k", json!(20)),
            ("min_p", json!("0")),
            ("presence_penalty", json!(" 1.5 ")),
            ("max_tokens", json!(4096)),
        ] {
            host.settings.insert(k.into(), v);
        }
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        let body = &host.chat_bodies.lock().unwrap()[0];
        assert!(body.get("temperature").is_none());
        assert_eq!(body["top_p"], 0.95);
        assert_eq!(body["top_k"], 20);
        assert_eq!(body["min_p"], 0.0);
        assert_eq!(body["presence_penalty"], 1.5);
        assert_eq!(body["max_tokens"], 4096);
        let events = host.events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|e| e["kind"] == "system"
                    && e["text"].as_str().unwrap().contains("temperature"))
        );
    }

    /// Crossing the cap trims well below it once, so the next turn's prompt
    /// starts with exactly the previous turn's messages (prefix-cache hit).
    #[test]
    fn history_trim_has_hysteresis_and_keeps_the_prefix_stable() {
        let host = FakeHost::new(vec![answer("ok")]);
        let seeded: Vec<Value> = (0..MAX_HISTORY_MESSAGES)
            .map(|i| {
                let role = if i % 2 == 0 { "user" } else { "assistant" };
                json!({ "role": role, "content": format!("m{i}") })
            })
            .collect();
        host.store.lock().unwrap().insert(
            (HISTORY_COLLECTION.to_string(), "s1".to_string()),
            Value::Array(seeded),
        );
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        let saved = host.saved_history();
        assert!(saved.len() <= HISTORY_TRIM_TARGET, "len {}", saved.len());
        assert_eq!(saved[0]["role"], "user");

        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        let bodies = host.chat_bodies.lock().unwrap();
        let first = bodies[0]["messages"].as_array().unwrap();
        let second = bodies[1]["messages"].as_array().unwrap();
        assert_eq!(&second[..first.len()], &first[..]);
    }

    #[test]
    fn tool_results_are_plain_text() {
        assert_eq!(tool_result_text(&json!("line 1\nline 2")), "line 1\nline 2");
        assert_eq!(
            tool_result_text(&json!({ "content": [
                { "type": "text", "text": "a\nb" },
                { "type": "image", "data": "AAAA" },
                { "type": "text", "text": "c" },
            ] })),
            "a\nb\nc"
        );
        assert_eq!(tool_result_text(&json!({ "n": 1 })), "{\n  \"n\": 1\n}");
    }

    /// A reply announcing a step it never took gets a persisted continue
    /// nudge, at most `MAX_CONTINUE_NUDGES` times per turn.
    #[test]
    fn announce_and_stop_is_nudged_at_most_twice() {
        let mut host = FakeHost::new(vec![answer("Found it.\nLet me check the video detail.")]);
        host.tool_defs = Some(json!([{ "name": "t", "input_schema": { "type": "object" } }]));
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        assert_eq!(
            host.chat_bodies.lock().unwrap().len(),
            1 + MAX_CONTINUE_NUDGES
        );
        let saved = host.saved_history();
        let nudges = saved
            .iter()
            .filter(|m| m["content"] == CONTINUE_NUDGE)
            .count();
        assert_eq!(nudges, MAX_CONTINUE_NUDGES);
        assert_eq!(saved.last().unwrap()["role"], "assistant");
        assert!(
            host.events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e["kind"] == "completed")
        );

        assert!(announces_unfinished_step("Now I’ll run the tests."));
        assert!(announces_unfinished_step("Here is the plan:\n\n"));
        assert!(!announces_unfinished_step("Let me know if you need more."));
        assert!(!announces_unfinished_step("All done."));
    }

    /// No nudge without tools: a chat-only model can't act on it.
    #[test]
    fn announce_without_tools_is_not_nudged() {
        let host = FakeHost::new(vec![answer("Let me think about that.")]);
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        assert_eq!(host.chat_bodies.lock().unwrap().len(), 1);
    }

    /// Real replies from a local agent that ended its turn without acting,
    /// next to finished answers that must not be nudged.
    #[test]
    fn announcement_detection_reads_the_last_sentence() {
        for stalled in [
            "Three questions, all read-only. Let me work through them.",
            "Let me answer all three.",
            "The video is still processing. Now let me check the worker logs.",
            "Next, I'll run the build:",
            "Okay, I'm going to restart the service.",
        ] {
            assert!(announces_unfinished_step(stalled), "{stalled}");
        }
        for done in [
            "The build passes: `Finished dev profile in 4.83s`.",
            "Fixed. Let me know if you want autoplay muted.",
            "Both thumbnails return 403, so the bucket still denies anonymous reads.",
        ] {
            assert!(!announces_unfinished_step(done), "{done}");
        }
    }

    #[test]
    fn length_finish_warns_about_the_token_limit() {
        let host = FakeHost::new(vec![finished(
            json!({ "role": "assistant", "content": "partial" }),
            10,
            2,
            "length",
        )]);
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        let events = host.events.lock().unwrap();
        assert!(events.iter().any(|e| e["kind"] == "system"
            && e["subtype"] == "warning"
            && e["text"].as_str().unwrap().contains("max-token limit")));
        assert!(events.iter().any(|e| e["kind"] == "completed"));
    }

    /// An oversized tool result reaches the model (and history) as head +
    /// tail around an omission marker; the UI's ToolEnd keeps all of it.
    #[test]
    fn long_tool_results_are_capped_for_the_model_only() {
        let full = format!("HEAD{}TAIL", "x".repeat(50_000));
        let mut host = FakeHost::new(vec![
            completion(
                json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "c1",
                        "type": "function",
                        "function": { "name": "t", "arguments": "{}" },
                    }],
                }),
                10,
                1,
            ),
            answer("done"),
        ]);
        host.tool_defs = Some(json!([{ "name": "t", "input_schema": { "type": "object" } }]));
        host.tool_result = json!(full);
        host.settings
            .insert("max_tool_result_chars".into(), json!(1000));
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();

        let bodies = host.chat_bodies.lock().unwrap();
        let msgs = bodies[1]["messages"].as_array().unwrap();
        let sent = msgs.last().unwrap()["content"].as_str().unwrap();
        assert!(sent.starts_with("HEAD") && sent.ends_with("TAIL"));
        assert!(sent.contains("[… 49,008 chars omitted: narrow the request"));
        assert!(sent.chars().count() < 1200);
        let events = host.events.lock().unwrap();
        let end = events.iter().find(|e| e["kind"] == "tool_end").unwrap();
        assert_eq!(end["output"].as_str().unwrap().len(), full.len());
        drop(events);
        let saved = host.saved_history();
        let tool = saved.iter().find(|m| m["role"] == "tool").unwrap();
        assert_eq!(tool["content"], sent);

        assert_eq!(cap_tool_result("ééééé", None), "ééééé");
        assert!(cap_tool_result(&"é".repeat(100), Some(10)).starts_with("ééééé\n"));
        assert_eq!(thousands(183_402), "183,402");
        assert_eq!(thousands(999), "999");
    }

    fn overflow() -> Value {
        json!({
            "status": 400,
            "body": json!({ "error": { "message": "This model's maximum context length is \
                262144 tokens. However, your prompt contains at least 262145 input tokens." } })
            .to_string(),
        })
    }

    fn seed_turns(host: &FakeHost, n: usize) {
        let seeded: Vec<Value> = (0..n)
            .map(|i| {
                let role = if i % 2 == 0 { "user" } else { "assistant" };
                json!({ "role": role, "content": format!("m{i}") })
            })
            .collect();
        host.store.lock().unwrap().insert(
            (HISTORY_COLLECTION.to_string(), "s1".to_string()),
            Value::Array(seeded),
        );
    }

    /// A context-length 400 trims about half the older turns, warns,
    /// persists the trimmed history and retries the round.
    #[test]
    fn context_overflow_trims_history_and_retries_once() {
        let host = FakeHost::new(vec![overflow(), answer("fits now")]);
        seed_turns(&host, 10);
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();

        let bodies = host.chat_bodies.lock().unwrap();
        assert_eq!(bodies.len(), 2);
        let before = bodies[0]["messages"].as_array().unwrap().len();
        let after = bodies[1]["messages"].as_array().unwrap();
        assert!(
            after.len() <= before / 2 + 1,
            "{} -> {}",
            before,
            after.len()
        );
        assert_eq!(after[1]["role"], "user");
        assert_eq!(after.last().unwrap()["content"], "hello");
        let events = host.events.lock().unwrap();
        assert!(
            events.iter().any(|e| e["kind"] == "system"
                && e["text"].as_str().unwrap().starts_with("History trimmed"))
        );
        assert!(events.iter().any(|e| e["kind"] == "completed"));
        drop(events);
        let saved = host.saved_history();
        assert_eq!(saved[0]["role"], "user");
        assert_eq!(saved.last().unwrap()["content"], "fits now");
        assert!(saved.len() < 10);
    }

    /// Still overflowing after the one trim: crash as before.
    #[test]
    fn second_context_overflow_crashes() {
        let host = FakeHost::new(vec![overflow()]);
        seed_turns(&host, 10);
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        assert_eq!(host.chat_bodies.lock().unwrap().len(), 2);
        let events = host.events.lock().unwrap();
        let crashed = events.iter().find(|e| e["kind"] == "crashed").unwrap();
        assert!(
            crashed["reason"]
                .as_str()
                .unwrap()
                .starts_with("openai-compat HTTP 400")
        );
    }

    fn offered_tool_names(host: &FakeHost) -> Vec<String> {
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        let bodies = host.chat_bodies.lock().unwrap();
        bodies[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap().to_string())
            .collect()
    }

    /// Tools go out sorted by name in both modes; Core keeps only the
    /// allowlisted names the session has, plus Extra Tools.
    #[test]
    fn tool_set_filters_to_core_plus_extra_and_sorts() {
        let defs = json!([
            { "name": "write_file", "input_schema": { "type": "object" } },
            { "name": "create_card", "input_schema": { "type": "object" } },
            { "name": "list_views", "input_schema": { "type": "object" } },
            { "name": "git", "input_schema": { "type": "object" } },
            { "name": "read_file", "input_schema": { "type": "object" } },
        ]);
        let mut all = FakeHost::new(vec![answer("ok")]);
        all.tool_defs = Some(defs.clone());
        assert_eq!(
            offered_tool_names(&all),
            [
                "create_card",
                "git",
                "list_views",
                "read_file",
                "write_file"
            ]
        );

        let mut core = FakeHost::new(vec![answer("ok")]);
        core.tool_defs = Some(defs);
        core.settings.insert("tool_set".into(), json!("core"));
        core.settings
            .insert("extra_tools".into(), json!(["create_card", "not_a_tool"]));
        assert_eq!(
            offered_tool_names(&core),
            ["create_card", "git", "read_file", "write_file"]
        );
    }
}
