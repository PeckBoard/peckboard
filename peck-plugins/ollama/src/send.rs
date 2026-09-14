//! Ollama `/api/chat` turn via `peckboard_http_request`.

use serde_json::{Value, json};

use crate::event::{CrashKind, ProviderEvent};
use crate::host::{self, HostFn};

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
        .unwrap_or("llama3.1");
    let model = model_raw
        .strip_prefix("ollama:")
        .unwrap_or(model_raw)
        .split('@')
        .next()
        .unwrap_or(model_raw);

    if should_stop(session_id) {
        return crash(session_id, "interrupted", CrashKind::Interrupted);
    }

    let base = setting_str("base_url").unwrap_or_else(|| "http://127.0.0.1:11434".into());
    let base = base.trim_end_matches('/').to_string();
    let url = format!("{base}/api/chat");

    let mut messages = Vec::new();
    if !system_prompt.is_empty() {
        messages.push(json!({ "role": "system", "content": system_prompt }));
    }
    let mut user = json!({ "role": "user", "content": prompt });
    if let Some(images) = image_attachments(&message) {
        user["images"] = json!(images);
    }
    messages.push(user);

    emit(
        session_id,
        &ProviderEvent::Started {
            model: model_raw.to_string(),
            conversation_id: payload
                .get("conversation_id")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            metadata: json!({ "provider": "ollama" }),
        },
    )?;

    let resp = host::call_host(
        HostFn::HttpRequest,
        &json!({
            "url": url,
            "method": "POST",
            "headers": { "Content-Type": "application/json" },
            "body": json!({
                "model": model,
                "messages": messages,
                "stream": false,
            }).to_string(),
            "timeout_secs": 300,
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
    if let Some(text) = msg
        .get("content")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        emit(
            session_id,
            &ProviderEvent::Text {
                text: text.to_string(),
            },
        )?;
    }
    let prompt_eval = parsed
        .get("prompt_eval_count")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let eval = parsed
        .get("eval_count")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
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
                model: Some(model.to_string()),
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

fn setting_str(key: &str) -> Option<String> {
    let v = host::call_host(HostFn::GetPluginSetting, &json!({ "key": key })).ok()?;
    v.get("value")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
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
