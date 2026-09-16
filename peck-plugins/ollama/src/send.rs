//! Ollama `/api/chat` turn via `peckboard_http_request`.
//!
//! Settings (`base_url`, named `servers`, headers, timeout, default model)
//! match the native provider. Multi-turn history lives in the plugin store
//! (Ollama is stateless). When `enable_tools` is on, each turn offers
//! Peckboard MCP tools and runs calls through `peckboard_provider_invoke_mcp`.

use serde_json::{Value, json};

use crate::event::{CrashKind, ProviderEvent};
use crate::host::{self, HostFn};
use crate::settings;

const HISTORY_COLLECTION: &str = "history";
/// Per-session `tool_use_id` allocator. Deliberately NOT the `history`
/// collection (which core wipes on session clear): persisted events outlive
/// the trimmed transcript, so ids must never repeat within a session.
const COUNTER_COLLECTION: &str = "counters";
const MAX_HISTORY_MESSAGES: usize = 100;
const MAX_TOOL_ROUNDS: usize = 16;
const CAPABILITY_PROBE_TIMEOUT_SECS: u64 = 15;

/// Per-turn token rollup. Ollama reports counts once per `/api/chat`
/// response, so a turn that ran tools produces one set per round; they sum
/// into the single `Usage` event the turn emits.
#[derive(Default)]
struct TurnUsage {
    /// Σ `prompt_eval_count` over the turn's rounds — every prompt token
    /// the server actually billed, replayed history included.
    input: i64,
    /// Σ `eval_count` over the turn's rounds.
    output: i64,
    /// `prompt_eval_count` of the last round that reported one: the context
    /// window at end of turn. Summing it would double-count the replayed
    /// transcript.
    context: i64,
}

impl TurnUsage {
    fn add_round(&mut self, prompt_eval: i64, eval: i64) {
        self.input += prompt_eval;
        self.output += eval;
        if prompt_eval > 0 {
            self.context = prompt_eval;
        }
    }

    fn is_empty(&self) -> bool {
        self.input == 0 && self.output == 0
    }
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
    let model_raw = cfg
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty() && *s != "default")
        .map(str::to_string)
        .or_else(|| settings::str("default_model"))
        .unwrap_or_else(|| "llama3.1".into());
    let (model, base) = match settings::resolve_model_ref(&model_raw) {
        Ok(resolved) => resolved,
        Err(e) => return crash(session_id, &e, CrashKind::Unknown),
    };
    // Usage rows carry the fully-qualified id, matching the old native
    // provider's `model_label` (the session's `ollama:`-prefixed selection).
    let model_label = if model_raw.starts_with("ollama:") {
        model_raw.clone()
    } else {
        format!("ollama:{model_raw}")
    };

    emit(
        session_id,
        &ProviderEvent::Started {
            model: model_raw.clone(),
            conversation_id: payload
                .get("conversation_id")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            metadata: json!({ "provider": "ollama" }),
        },
    )?;

    let headers = settings::http_headers();

    // Probe what this model actually supports before building the request.
    // Ollama 400s a `/api/chat` that carries `tools` for a model whose
    // template has no tool support (e.g. gemma-based models), and a
    // non-vision model has nowhere to put images. A failed probe falls back
    // to permissive defaults so an unreachable `/api/show` never breaks an
    // otherwise-working setup. One probe per turn (the plugin has no clock
    // on wasm32-unknown-unknown, so the old 30 s TTL degrades to this).
    let caps = model_capabilities(&base, &model, &headers);
    let supports_tools = capability_present(&caps, "tools");
    let supports_vision = capability_present(&caps, "vision");
    let supports_thinking = capability_present(&caps, "thinking");

    // Only forward images to a model that advertises vision; a text-only
    // model would reject or silently mangle them.
    let images = image_attachments(&message);
    let images = if supports_vision {
        images
    } else {
        if images.is_some() {
            emit(
                session_id,
                &ProviderEvent::System {
                    text: format!(
                        "Model '{model}' does not advertise vision support; \
                         image attachment(s) were dropped."
                    ),
                    subtype: "warning".into(),
                    detail: Value::Null,
                },
            )?;
        }
        None
    };

    // Durable history is user-first. Earlier plugin versions persisted the
    // `role:"system"` message, freezing the first turn's prompt forever —
    // strip it and prepend the CURRENT system prompt to the request only.
    let mut history = load_history(session_id);
    if history
        .first()
        .and_then(|m| m.get("role").and_then(|r| r.as_str()))
        == Some("system")
    {
        history.remove(0);
    }
    let mut user = json!({ "role": "user", "content": prompt });
    if let Some(imgs) = images {
        user["images"] = json!(imgs);
    }
    history.push(user);
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

    let tools = if settings::bool("enable_tools", true) && supports_tools {
        ollama_tools(session_id)
    } else {
        None
    };
    let url = format!("{base}/api/chat");
    let mut usage = TurnUsage::default();

    for round in 0..=MAX_TOOL_ROUNDS {
        if should_stop(session_id) {
            return interrupted_exit(session_id, &model_label, history, new_messages, &usage);
        }
        let mut body = json!({
            "model": model,
            "messages": messages,
            "stream": false,
        });
        if supports_thinking {
            body["think"] = json!(true);
        }
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
            emit_usage(session_id, &model_label, &usage)?;
            return crash(
                session_id,
                &format!(
                    "ollama HTTP {status}: {}",
                    body.chars().take(2_000).collect::<String>()
                ),
                CrashKind::Unknown,
            );
        }
        // An empty or unparseable 2xx body means the server closed the
        // connection before producing anything — the one genuine NoOutput.
        let parsed: Value = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(_) => {
                emit_usage(session_id, &model_label, &usage)?;
                return crash(
                    session_id,
                    "Ollama closed the stream before producing output",
                    CrashKind::NoOutput,
                );
            }
        };
        // Counts ride the same body; accumulate before the error check so a
        // response that also carries an error still contributes what the
        // server billed.
        usage.add_round(
            parsed
                .get("prompt_eval_count")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            parsed
                .get("eval_count")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
        );
        if let Some(e) = parsed.get("error").and_then(|v| v.as_str()) {
            emit_usage(session_id, &model_label, &usage)?;
            return crash(session_id, e, CrashKind::Unknown);
        }
        let Some(msg) = parsed.get("message").cloned() else {
            emit_usage(session_id, &model_label, &usage)?;
            return crash(
                session_id,
                "Ollama closed the stream before producing output",
                CrashKind::NoOutput,
            );
        };
        if let Some(think) = msg
            .get("thinking")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            emit(
                session_id,
                &ProviderEvent::Thinking {
                    text: think.to_string(),
                },
            )?;
        }
        let last_text = msg
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if !last_text.is_empty() {
            emit(
                session_id,
                &ProviderEvent::Text {
                    text: last_text.clone(),
                },
            )?;
        }
        let last_tool_calls = msg
            .get("tool_calls")
            .cloned()
            .filter(|v| v.as_array().is_some_and(|a| !a.is_empty()));
        let mut assistant = json!({ "role": "assistant", "content": last_text });
        if let Some(tc) = &last_tool_calls {
            assistant["tool_calls"] = tc.clone();
        }
        messages.push(assistant.clone());
        new_messages.push(assistant);
        // The stop may have landed while the (blocking) request was in
        // flight: persist what we have instead of dropping the exchange.
        if should_stop(session_id) {
            return interrupted_exit(session_id, &model_label, history, new_messages, &usage);
        }
        let Some(calls) = last_tool_calls else {
            break;
        };
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
    }

    persist_turn(session_id, &mut history, new_messages);
    emit_usage(session_id, &model_label, &usage)?;
    emit(
        session_id,
        &ProviderEvent::Completed {
            conversation_id: payload
                .get("conversation_id")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            result_meta: Value::Null,
        },
    )?;
    Ok(())
}

/// `/api/show` capabilities for `model` on `base` (e.g.
/// `["completion","tools","vision","thinking"]`). `None` on any failure —
/// callers treat that as "unknown" and fall back to permissive defaults.
fn model_capabilities(
    base: &str,
    model: &str,
    headers: &serde_json::Map<String, Value>,
) -> Option<Vec<String>> {
    let resp = host::call_host(
        HostFn::HttpRequest,
        &json!({
            "url": format!("{base}/api/show"),
            "method": "POST",
            "headers": headers,
            "body": json!({ "model": model }).to_string(),
            "timeout_secs": CAPABILITY_PROBE_TIMEOUT_SECS,
        }),
    )
    .ok()?;
    let status = resp.get("status").and_then(|v| v.as_u64()).unwrap_or(0);
    if !(200..300).contains(&status) {
        return None;
    }
    let body = resp.get("body").and_then(|v| v.as_str())?;
    let parsed: Value = serde_json::from_str(body).ok()?;
    Some(
        parsed
            .get("capabilities")
            .and_then(|c| c.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    )
}

/// Whether the probed `capabilities` contain `cap`. `None` means the probe
/// failed / capabilities are unknown — treated as permissive (`true`) so an
/// unreachable `/api/show` never strips tools or images from a model that
/// would otherwise have worked.
fn capability_present(capabilities: &Option<Vec<String>>, cap: &str) -> bool {
    match capabilities {
        Some(caps) => caps.iter().any(|c| c == cap),
        None => true,
    }
}

/// Emit the turn's single `Usage` event, or nothing when the server
/// reported no counts. Ollama has no prompt caching, so both cache fields
/// are 0 and `total` is the context window plus the generated output.
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
    history.extend(new_messages);
    save_history(session_id, history);
    crash(session_id, "interrupted", CrashKind::Interrupted)
}

/// Append this turn's generated messages to the durable transcript.
fn persist_turn(session_id: &str, history: &mut Vec<Value>, new_messages: Vec<Value>) {
    history.extend(new_messages);
    save_history(session_id, history.clone());
}

fn ollama_tools(session_id: &str) -> Option<Value> {
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

/// Reserve `count` consecutive `tool_use_id` ordinals for this session,
/// bumping the durable counter BEFORE any id is emitted so a crash mid-round
/// can never hand the same id out twice.
fn alloc_tool_ids(session_id: &str, count: u64) -> u64 {
    let cur = host::call_host(
        HostFn::StoreGet,
        &json!({ "collection": COUNTER_COLLECTION, "key": session_id }),
    )
    .ok()
    .and_then(|v| v.get("value").and_then(|x| x.as_u64()))
    .unwrap_or(0);
    let _ = host::call_host(
        HostFn::StorePut,
        &json!({
            "collection": COUNTER_COLLECTION,
            "key": session_id,
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
    calls: &Value,
    messages: &mut Vec<Value>,
    new_messages: &mut Vec<Value>,
) -> Result<bool, String> {
    let Some(arr) = calls.as_array() else {
        return Ok(false);
    };
    let base = alloc_tool_ids(session_id, arr.len() as u64);
    for (i, call) in arr.iter().enumerate() {
        if should_stop(session_id) {
            return Ok(true);
        }
        let fn_obj = call.get("function").unwrap_or(call);
        let name = fn_obj
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        let args = fn_obj.get("arguments").cloned().unwrap_or(json!({}));
        let tool_id = format!("ollama-tool-{}", base + i as u64);
        emit(
            session_id,
            &ProviderEvent::ToolStart {
                tool_use_id: tool_id.clone(),
                name: name.clone(),
                input: args.clone(),
            },
        )?;
        let invoked = host::call_host(
            HostFn::ProviderInvokeMcp,
            &json!({
                "session_id": session_id,
                "name": name,
                "arguments": args,
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
            "tool_name": name,
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
        // a dangling reference Ollama can reject.
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

fn image_attachments(message: &Value) -> Option<Vec<String>> {
    let atts = message.get("attachments")?.as_array()?;
    let imgs: Vec<String> = atts
        .iter()
        .filter(|a| {
            a.get("mime_type")
                .and_then(|v| v.as_str())
                .is_some_and(|m| m.starts_with("image/"))
        })
        .filter_map(|a| {
            a.get("data_base64")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect();
    if imgs.is_empty() { None } else { Some(imgs) }
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
    /// touches. `stop_from` = the 1-based `should_stop` call number from
    /// which the answer flips to `true` (0 = never).
    struct FakeHost {
        store: Arc<Mutex<HashMap<(String, String), Value>>>,
        events: Arc<Mutex<Vec<Value>>>,
        chat_bodies: Arc<Mutex<Vec<Value>>>,
        chat_response: Value,
        stop_from: usize,
        stop_calls: Arc<Mutex<usize>>,
        show_response: Value,
    }

    impl FakeHost {
        fn new(chat_response: Value) -> Self {
            FakeHost {
                store: Arc::default(),
                events: Arc::default(),
                chat_bodies: Arc::default(),
                chat_response,
                stop_from: 0,
                stop_calls: Arc::default(),
                show_response: json!({
                    "status": 200,
                    "body": json!({"capabilities": ["completion", "tools"]}).to_string(),
                }),
            }
        }

        fn host_fn(&self) -> peck_plugin_native_host::HostFn {
            let store = self.store.clone();
            let events = self.events.clone();
            let chat_bodies = self.chat_bodies.clone();
            let chat_response = self.chat_response.clone();
            let stop_from = self.stop_from;
            let stop_calls = self.stop_calls.clone();
            let show_response = self.show_response.clone();
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
                    "peckboard_provider_get_mcp_config" => {
                        json!({ "error": "no mcp in test" }).to_string()
                    }
                    "peckboard_http_request" => {
                        let url = input.get("url").and_then(|u| u.as_str()).unwrap_or("");
                        if url.ends_with("/api/show") {
                            show_response.to_string()
                        } else {
                            let body = input.get("body").and_then(|b| b.as_str()).unwrap_or("{}");
                            chat_bodies
                                .lock()
                                .unwrap()
                                .push(serde_json::from_str(body).unwrap_or(json!({})));
                            chat_response.to_string()
                        }
                    }
                    other => json!({ "error": format!("unexpected host fn {other}") }).to_string(),
                }
            })
        }
    }

    fn payload() -> Value {
        json!({
            "session_id": "s1",
            "spawn_config": { "model": "ollama:llama3.1" },
            "message": { "text": "hello" },
            "system_prompt": "NEW PROMPT",
        })
    }

    fn seed_history(host: &FakeHost, messages: Value) {
        host.store
            .lock()
            .unwrap()
            .insert((HISTORY_COLLECTION.to_string(), "s1".to_string()), messages);
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
        let host = FakeHost::new(json!({
            "status": 200,
            "body": json!({
                "message": { "role": "assistant", "content": "answer" },
                "done": true,
                "prompt_eval_count": 50,
                "eval_count": 7,
            })
            .to_string(),
        }));
        seed_history(
            &host,
            json!([
                { "role": "system", "content": "OLD PROMPT" },
                { "role": "user", "content": "q1" },
                { "role": "assistant", "content": "a1" },
            ]),
        );
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();

        let bodies = host.chat_bodies.lock().unwrap();
        let msgs = bodies[0]["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "NEW PROMPT");
        assert_eq!(msgs[1]["content"], "q1");
        assert!(!bodies[0].to_string().contains("OLD PROMPT"));

        let store = host.store.lock().unwrap();
        let saved = store
            .get(&(HISTORY_COLLECTION.to_string(), "s1".to_string()))
            .unwrap()
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(saved.first().unwrap()["role"], "user");
        assert!(!saved.iter().any(|m| m["role"] == "system"));
        assert_eq!(saved.last().unwrap()["content"], "answer");

        // Usage: ollama:-prefixed label, total = context + output.
        let events = host.events.lock().unwrap();
        let usage = events.iter().find(|e| e["kind"] == "usage").unwrap();
        assert_eq!(usage["model"], "ollama:llama3.1");
        assert_eq!(usage["context_tokens"], 50);
        assert_eq!(usage["total_tokens"], 57);
        assert!(events.iter().any(|e| e["kind"] == "completed"));
    }

    /// An empty/unparseable 2xx body is a NoOutput crash, not a silent
    /// completion with an empty message.
    #[test]
    fn empty_response_crashes_with_no_output() {
        let host = FakeHost::new(json!({ "status": 200, "body": "" }));
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        let events = host.events.lock().unwrap();
        let crashed = events.iter().find(|e| e["kind"] == "crashed").unwrap();
        assert_eq!(crashed["error_kind"], "no_output");
        assert_eq!(
            crashed["reason"],
            "Ollama closed the stream before producing output"
        );
        assert!(!events.iter().any(|e| e["kind"] == "completed"));
    }

    /// A stop landing while the request was in flight persists the partial
    /// exchange with the interrupt marker and emits the accumulated usage.
    #[test]
    fn interrupt_persists_marked_partial_and_usage() {
        let mut host = FakeHost::new(json!({
            "status": 200,
            "body": json!({
                "message": { "role": "assistant", "content": "half an answer" },
                "done": true,
                "prompt_eval_count": 11,
                "eval_count": 4,
            })
            .to_string(),
        }));
        // First should_stop (loop top) passes; second (post-response) stops.
        host.stop_from = 2;
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();

        let events = host.events.lock().unwrap();
        let crashed = events.iter().find(|e| e["kind"] == "crashed").unwrap();
        assert_eq!(crashed["error_kind"], "interrupted");
        let usage = events.iter().find(|e| e["kind"] == "usage").unwrap();
        assert_eq!(usage["total_tokens"], 15);

        let store = host.store.lock().unwrap();
        let saved = store
            .get(&(HISTORY_COLLECTION.to_string(), "s1".to_string()))
            .unwrap()
            .as_array()
            .unwrap()
            .clone();
        let last = saved.last().unwrap();
        assert_eq!(last["role"], "assistant");
        assert_eq!(
            last["content"],
            "half an answer\n\n[Response interrupted by user]"
        );
    }

    /// tool_use_ids come from a durable per-session counter, so they never
    /// repeat across rounds or turns (`ollama-tool-{i}` reset per round made
    /// the frontend drop tool blocks with duplicate ids).
    #[test]
    fn tool_ids_are_unique_across_rounds_and_turns() {
        let host = FakeHost::new(json!({ "status": 200, "body": "" }));
        let (a, b) = peck_plugin_native_host::with_host(host.host_fn(), || {
            (alloc_tool_ids("s1", 3), alloc_tool_ids("s1", 2))
        });
        assert_eq!(a, 0);
        assert_eq!(b, 3);
    }
}
