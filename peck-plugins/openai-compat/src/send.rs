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
const MAX_HISTORY_MESSAGES: usize = 100;
const MAX_TOOL_ROUNDS: usize = 16;
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
    // forget the user said anything.
    save_history(session_id, history.clone());

    let mut messages = history.clone();
    if !system_prompt.trim().is_empty() {
        messages.insert(0, json!({ "role": "system", "content": system_prompt }));
    }
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

    let mut round = 0;
    while round <= MAX_TOOL_ROUNDS {
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
            // A server/model without tool calling (e.g. vLLM without
            // --enable-auto-tool-choice) 400s a request carrying `tools`:
            // drop them for the rest of the turn and retry this round once.
            if status == 400 && tools.is_some() && body.to_ascii_lowercase().contains("tool") {
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
        let text = msg
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if !text.is_empty() {
            emit(session_id, &ProviderEvent::Text { text: text.clone() })?;
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
            break;
        }
        if round == MAX_TOOL_ROUNDS {
            persist_turn(session_id, &mut history, new_messages);
            emit_usage(session_id, &model_label, &usage)?;
            return crash(
                session_id,
                "model kept calling tools past the per-turn cap",
                CrashKind::Unknown,
            );
        }
        if run_tool_calls(session_id, &calls, &mut messages, &mut new_messages)? {
            return interrupted_exit(session_id, &model_label, history, new_messages, &usage);
        }
        round += 1;
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
    save_history(session_id, history);
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
    save_history(session_id, history.clone());
}

/// Peckboard MCP tools as OpenAI `{"type":"function","function":{…}}` defs.
fn function_tools(session_id: &str) -> Option<Value> {
    let mcp = host::call_host(
        HostFn::ProviderGetMcpConfig,
        &json!({ "session_id": session_id }),
    )
    .ok()?;
    let defs = mcp.get("tool_defs")?.as_array()?;
    let tools: Vec<Value> = defs
        .iter()
        .filter_map(|d| {
            let name = d.get("name")?.as_str()?.to_string();
            if name.is_empty() {
                return None;
            }
            Some(json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": d.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                    "parameters": d.get("input_schema").cloned().unwrap_or(json!({"type": "object"})),
                }
            }))
        })
        .collect();
    if tools.is_empty() {
        None
    } else {
        Some(Value::Array(tools))
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
                (result.to_string(), true)
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
        let tool_msg = json!({
            "role": "tool",
            "tool_call_id": call.wire_id,
            "content": content,
        });
        messages.push(tool_msg.clone());
        new_messages.push(tool_msg);
    }
    Ok(false)
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

fn save_history(session_id: &str, mut messages: Vec<Value>) {
    if messages.len() > MAX_HISTORY_MESSAGES {
        let drop = messages.len() - MAX_HISTORY_MESSAGES;
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
                    "peckboard_get_plugin_setting" => json!({ "value": null }).to_string(),
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
                        json!({ "ok": true, "result": { "files": ["a.rs"] } }).to_string()
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
        json!({
            "status": 200,
            "body": json!({
                "choices": [{ "index": 0, "message": message, "finish_reason": "stop" }],
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
        assert_eq!(tool["content"], json!({ "files": ["a.rs"] }).to_string());

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
}
