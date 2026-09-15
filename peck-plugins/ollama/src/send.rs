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
const MAX_HISTORY_MESSAGES: usize = 100;
const MAX_TOOL_ROUNDS: usize = 16;

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
    let (model, base) = settings::resolve_model_ref(&model_raw);

    if should_stop(session_id) {
        return crash(session_id, "interrupted", CrashKind::Interrupted);
    }

    let url = format!("{base}/api/chat");
    let mut messages = load_history(session_id);
    if messages
        .first()
        .and_then(|m| m.get("role").and_then(|r| r.as_str()))
        != Some("system")
        && !system_prompt.is_empty()
    {
        messages.insert(0, json!({ "role": "system", "content": system_prompt }));
    }
    let mut user = json!({ "role": "user", "content": prompt });
    if let Some(images) = image_attachments(&message) {
        user["images"] = json!(images);
    }
    messages.push(user);

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

    let tools = if settings::bool("enable_tools", true) {
        ollama_tools(session_id)
    } else {
        None
    };
    let mut headers = settings::http_headers();
    headers.insert("Content-Type".into(), json!("application/json"));
    let mut prompt_eval = 0i64;
    let mut eval = 0i64;

    for round in 0..=MAX_TOOL_ROUNDS {
        if should_stop(session_id) {
            return crash(session_id, "interrupted", CrashKind::Interrupted);
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
        if should_stop(session_id) {
            return crash(session_id, "interrupted", CrashKind::Interrupted);
        }
        let status = resp.get("status").and_then(|v| v.as_u64()).unwrap_or(0);
        let body = resp.get("body").and_then(|v| v.as_str()).unwrap_or("");
        if !(200..300).contains(&status) {
            return crash(
                session_id,
                &format!(
                    "ollama HTTP {status}: {}",
                    body.chars().take(400).collect::<String>()
                ),
                CrashKind::Unknown,
            );
        }
        let parsed: Value = serde_json::from_str(body).unwrap_or(json!({}));
        if let Some(e) = parsed.get("error").and_then(|v| v.as_str()) {
            return crash(session_id, e, CrashKind::Unknown);
        }
        prompt_eval += parsed
            .get("prompt_eval_count")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        eval += parsed
            .get("eval_count")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let msg = parsed.get("message").cloned().unwrap_or(json!({}));
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
        messages.push(assistant);
        let Some(calls) = last_tool_calls else {
            break;
        };
        if round == MAX_TOOL_ROUNDS {
            return crash(
                session_id,
                "model kept calling tools past the per-turn cap",
                CrashKind::Unknown,
            );
        }
        run_tool_calls(session_id, &calls, &mut messages)?;
    }

    save_history(session_id, messages);
    if prompt_eval > 0 || eval > 0 {
        emit(
            session_id,
            &ProviderEvent::Usage {
                input_tokens: prompt_eval,
                output_tokens: eval,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                total_tokens: prompt_eval + eval,
                context_tokens: prompt_eval,
                model: Some(model),
                turn_seq: None,
            },
        )?;
    }
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

fn run_tool_calls(
    session_id: &str,
    calls: &Value,
    messages: &mut Vec<Value>,
) -> Result<(), String> {
    let Some(arr) = calls.as_array() else {
        return Ok(());
    };
    for (i, call) in arr.iter().enumerate() {
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
        let tool_id = format!("ollama-tool-{i}");
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
        messages.push(json!({
            "role": "tool",
            "tool_name": name,
            "content": content,
        }));
    }
    Ok(())
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
        let keep_system = messages
            .first()
            .and_then(|m| m.get("role").and_then(|r| r.as_str()))
            == Some("system");
        let start = if keep_system { 1 } else { 0 };
        messages.drain(start..start + drop.min(messages.len().saturating_sub(start)));
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
