//! Drive one Claude CLI turn through host spawn / read_line / stdin.

use serde_json::{Value, json};

use crate::argv::{self, CliSpec};
use crate::event::{CrashKind, ProviderEvent};
use crate::host::{self, HostFn};
use crate::parser::{self, ParserState};
use crate::sandbox;
use crate::usage::UsageTracker;

pub fn run(payload: &Value) -> Result<(), String> {
    let session_id = payload
        .get("session_id")
        .and_then(|v| v.as_str())
        .ok_or("provider.send payload missing session_id")?;
    let cfg = payload.get("spawn_config").cloned().unwrap_or(json!({}));
    let message = payload.get("message").cloned().unwrap_or(json!({}));
    let conversation_id = payload
        .get("conversation_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let system_prompt = payload
        .get("system_prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let working_dir = cfg
        .get("working_dir")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if working_dir.is_empty() {
        return Err("spawn_config.working_dir is empty".into());
    }

    let account = host::call_host(
        HostFn::ProviderAccountEnv,
        &json!({ "session_id": session_id, "model": cfg.get("model") }),
    )?;
    let mut env = map_string_map(cfg.get("env"));
    if let Some(extra) = account.get("env").and_then(|v| v.as_object()) {
        for (k, v) in extra {
            if let Some(s) = v.as_str() {
                env.insert(k.clone(), s.to_string());
            }
        }
    }
    let mut env_remove: Vec<String> = account
        .get("env_remove")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let mcp = host::call_host(
        HostFn::ProviderGetMcpConfig,
        &json!({ "session_id": session_id }),
    )
    .unwrap_or(json!({}));
    let mcp_path = mcp
        .get("path")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            cfg.get("mcp_config_path")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });
    let core_tools = string_list(mcp.get("core_tools"));
    let pre_hatcher_tools = string_list(mcp.get("pre_hatcher_tools"));

    let _ = host::call_host(
        HostFn::ProviderWriteFile,
        &json!({
            "session_id": session_id,
            "path": argv::HOOK_CONTEXT_FILE,
            "contents": argv::subagent_context_json(),
        }),
    );
    let subagent_path = format!("{working_dir}/{}", argv::HOOK_CONTEXT_FILE);

    let spec = CliSpec {
        model: cfg
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string(),
        effort: cfg
            .get("effort")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        conversation_id: conversation_id.clone(),
        mcp_config_path: mcp_path,
        permission_mode: cfg
            .get("permission_mode")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        is_worker: cfg
            .get("is_worker")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        is_pre_hatcher: cfg
            .get("is_pre_hatcher")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        extra_allowed_tools: string_list(cfg.get("extra_allowed_tools")),
        extra_disallowed_tools: string_list(cfg.get("extra_disallowed_tools")),
        system_prompt,
        core_tools,
        pre_hatcher_tools,
        subagent_context_path: Some(subagent_path),
    };
    let args = argv::build_cli_args(&spec);

    host::call_host(
        HostFn::ProviderSpawn,
        &json!({
            "session_id": session_id,
            "command": "claude",
            "args": args,
            "env": env,
            "env_remove": env_remove,
            "cwd": working_dir,
        }),
    )?;

    let frame = argv::build_user_message_frame(&message);
    write_stdin(session_id, &frame)?;

    let mut parser = ParserState::new();
    let mut usage = UsageTracker::default();
    let mut interrupt_sent = false;
    let mut interrupt_id: Option<String> = None;
    let mut saw_result = false;
    let mut last_result_error: Option<String> = None;
    let mut last_result_json: Option<Value> = None;

    loop {
        if !interrupt_sent && should_stop(session_id) {
            interrupt_sent = true;
            interrupt_id = Some("pb-interrupt-1".into());
            let frame = json!({
                "type": "control_request",
                "request_id": "pb-interrupt-1",
                "request": { "subtype": "interrupt" },
            });
            let _ = write_stdin(session_id, &frame.to_string());
        }

        if let Some(msg) = take_message(session_id)? {
            let frame = argv::build_user_message_frame(&msg);
            let _ = write_stdin(session_id, &frame);
            last_result_error = None;
            parser.reset_turn();
        }

        let line = read_line(session_id, 100)?;
        if line
            .get("stopped")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            if !interrupt_sent {
                let _ = kill(session_id);
            }
            continue;
        }
        if line
            .get("timeout")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            if interrupt_sent {
                let _ = kill(session_id);
            }
            continue;
        }
        if line.get("eof").and_then(|v| v.as_bool()).unwrap_or(false) {
            let exit_code = line
                .get("exit_code")
                .and_then(|v| v.as_i64())
                .map(|n| n as i32);
            let stderr = line
                .get("stderr")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            finish(
                session_id,
                &mut parser,
                &mut usage,
                saw_result,
                last_result_error,
                last_result_json.as_ref(),
                interrupt_sent,
                exit_code,
                stderr,
            )?;
            return Ok(());
        }
        let Some(raw) = line.get("line").and_then(|v| v.as_str()) else {
            continue;
        };
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let json_line: Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if json_line.get("type").and_then(|v| v.as_str()) == Some("control_response") {
            let rid = json_line
                .pointer("/response/request_id")
                .and_then(|v| v.as_str());
            if interrupt_id.as_deref() == rid {
                let subtype = json_line
                    .pointer("/response/subtype")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if subtype == "error" {
                    let _ = kill(session_id);
                }
            }
            continue;
        }

        if json_line.get("type").and_then(|v| v.as_str()) == Some("control_request") {
            handle_control(session_id, &json_line, &working_dir)?;
            continue;
        }

        usage.observe_line(&json_line);
        let is_result = json_line.get("type").and_then(|v| v.as_str()) == Some("result");
        let events = parser::parse_stream_json(&json_line, &mut parser);
        for ev in events {
            emit(session_id, &ev)?;
        }
        if is_result {
            saw_result = true;
            last_result_error = result_error(&json_line);
            last_result_json = Some(json_line.clone());
            let usages = usage.on_result(&json_line, parser.model_name.as_deref());
            for u in usages {
                emit(
                    session_id,
                    &ProviderEvent::Usage {
                        input_tokens: u.slices.input,
                        output_tokens: u.slices.output,
                        cache_read_tokens: u.slices.cache_read,
                        cache_creation_tokens: u.slices.cache_creation,
                        total_tokens: u.slices.total(),
                        context_tokens: u.context_tokens,
                        model: u.model,
                        turn_seq: None,
                    },
                )?;
            }
            let mut meta = result_meta(&json_line);
            if let Some(err) = last_result_error.as_ref() {
                if meta.is_null() {
                    meta = json!({});
                }
                if let Some(obj) = meta.as_object_mut() {
                    obj.insert("error".into(), json!(err));
                    if err.to_ascii_lowercase().contains("auth")
                        || err.to_ascii_lowercase().contains("login")
                        || err.contains("401")
                    {
                        obj.insert("error_kind".into(), json!("auth_expired"));
                    }
                }
            }
            emit(
                session_id,
                &ProviderEvent::Completed {
                    conversation_id: parser.conversation_id.clone(),
                    result_meta: meta,
                },
            )?;
            parser.reset_turn();
            // One provider.send = one turn. The CLI child is dropped when
            // we return; next send respawns (or resumes via --resume).
            let _ = kill(session_id);
            return Ok(());
        }
    }
}

fn finish(
    session_id: &str,
    parser: &mut ParserState,
    usage: &mut UsageTracker,
    saw_result: bool,
    last_result_error: Option<String>,
    _last_result: Option<&Value>,
    interrupted: bool,
    exit_code: Option<i32>,
    stderr: Option<String>,
) -> Result<(), String> {
    if saw_result && last_result_error.is_none() && !interrupted {
        return Ok(());
    }
    for u in usage.take_crash_fallback(parser.model_name.as_deref()) {
        emit(
            session_id,
            &ProviderEvent::Usage {
                input_tokens: u.slices.input,
                output_tokens: u.slices.output,
                cache_read_tokens: u.slices.cache_read,
                cache_creation_tokens: u.slices.cache_creation,
                total_tokens: u.slices.total(),
                context_tokens: u.context_tokens,
                model: u.model,
                turn_seq: None,
            },
        )?;
    }
    let (reason, kind) = if interrupted {
        ("interrupted".into(), CrashKind::Interrupted)
    } else if let Some(err) = last_result_error {
        let kind = if err.to_ascii_lowercase().contains("auth")
            || err.to_ascii_lowercase().contains("login")
        {
            CrashKind::AuthExpired
        } else {
            CrashKind::Unknown
        };
        (err, kind)
    } else {
        (
            stderr
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "claude exited without a result".into()),
            CrashKind::NoOutput,
        )
    };
    emit(
        session_id,
        &ProviderEvent::Crashed {
            reason,
            error_kind: kind,
            exit_code,
            stderr,
        },
    )
}

fn handle_control(session_id: &str, json: &Value, allowed_dir: &str) -> Result<(), String> {
    let request = json.get("request");
    let request_id = json
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let subtype = request
        .and_then(|r| r.get("subtype"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let tool_name = request
        .and_then(|r| r.get("tool_name"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if subtype != "can_use_tool" {
        return Ok(());
    }
    if tool_name == "AskUserQuestion" {
        let input = request.and_then(|r| r.get("input"));
        let questions = parser::normalize_questions(input);
        let tool_use_id = request
            .and_then(|r| r.get("tool_use_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        emit(
            session_id,
            &ProviderEvent::ControlRequest {
                request_id: request_id.to_string(),
                request_type: "AskUserQuestion".into(),
                payload: json!({
                    "toolUseId": tool_use_id,
                    "questions": questions,
                }),
            },
        )?;
        return Ok(());
    }
    let input = request
        .and_then(|r| r.get("input"))
        .cloned()
        .unwrap_or(json!({}));
    let denied = sandbox::check_path_violation(tool_name, &input, allowed_dir);
    let response = if let Some(reason) = denied {
        json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": { "behavior": "deny", "message": reason },
            }
        })
    } else {
        json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": { "behavior": "allow", "updatedInput": input },
            }
        })
    };
    write_stdin(session_id, &response.to_string())?;
    Ok(())
}

fn emit(session_id: &str, event: &ProviderEvent) -> Result<(), String> {
    let value = serde_json::to_value(event).map_err(|e| e.to_string())?;
    host::call_host(
        HostFn::EmitProviderEvent,
        &json!({ "session_id": session_id, "event": value }),
    )
    .map(|_| ())
}

fn write_stdin(session_id: &str, text: &str) -> Result<(), String> {
    let mut line = text.to_string();
    if !line.ends_with('\n') {
        line.push('\n');
    }
    host::call_host(
        HostFn::ProviderWriteStdin,
        &json!({ "session_id": session_id, "text": line }),
    )
    .map(|_| ())
}

fn read_line(session_id: &str, timeout_ms: u64) -> Result<Value, String> {
    host::call_host(
        HostFn::ProviderReadLine,
        &json!({ "session_id": session_id, "timeout_ms": timeout_ms }),
    )
}

fn kill(session_id: &str) -> Result<(), String> {
    host::call_host(HostFn::ProviderKill, &json!({ "session_id": session_id })).map(|_| ())
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

fn take_message(session_id: &str) -> Result<Option<Value>, String> {
    let v = host::call_host(
        HostFn::ProviderTakeMessage,
        &json!({ "session_id": session_id }),
    )?;
    Ok(v.get("message").cloned().filter(|m| !m.is_null()))
}

fn result_error(json: &Value) -> Option<String> {
    let is_error = json
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let subtype = json.get("subtype").and_then(|v| v.as_str());
    if !is_error && subtype.is_none_or(|s| s == "success") {
        return None;
    }
    Some(
        json.get("result")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .or_else(|| subtype.map(str::to_string))
            .unwrap_or_else(|| "the model reported an error".into()),
    )
}

fn result_meta(json: &Value) -> Value {
    let mut meta = serde_json::Map::new();
    if let Some(v) = json.get("duration_ms").and_then(|v| v.as_i64()) {
        meta.insert("durationMs".into(), v.into());
    }
    if let Some(v) = json.get("num_turns").and_then(|v| v.as_i64()) {
        meta.insert("numTurns".into(), v.into());
    }
    if let Some(n) = json
        .get("total_cost_usd")
        .and_then(|v| v.as_f64())
        .and_then(serde_json::Number::from_f64)
    {
        meta.insert("totalCostUsd".into(), Value::Number(n));
    }
    if let Some(denials) = json
        .get("permission_denials")
        .and_then(|v| v.as_array())
        .filter(|d| !d.is_empty())
    {
        meta.insert("permissionDenials".into(), Value::Array(denials.clone()));
    }
    if meta.is_empty() {
        Value::Null
    } else {
        Value::Object(meta)
    }
}

fn string_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn map_string_map(v: Option<&Value>) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    if let Some(obj) = v.and_then(|v| v.as_object()) {
        for (k, val) in obj {
            if let Some(s) = val.as_str() {
                out.insert(k.clone(), s.to_string());
            }
        }
    }
    out
}
