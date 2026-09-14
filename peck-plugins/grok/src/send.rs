//! One-shot grok CLI turn.

use serde_json::{Value, json};

use crate::argv;
use crate::event::{CrashKind, ProviderEvent};
use crate::host::{self, HostFn};
use crate::mcp;
use crate::parser::{self, UsageTracker};

pub fn run(payload: &Value) -> Result<(), String> {
    let session_id = payload
        .get("session_id")
        .and_then(|v| v.as_str())
        .ok_or("provider.send payload missing session_id")?;
    let cfg = payload.get("spawn_config").cloned().unwrap_or(json!({}));
    let message = payload.get("message").cloned().unwrap_or(json!({}));
    let prompt = message
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
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
    let model = cfg.get("model").and_then(|v| v.as_str()).unwrap_or("");

    let account = host::call_host(
        HostFn::ProviderAccountEnv,
        &json!({ "session_id": session_id, "model": model }),
    )?;
    let mut env = map_env(cfg.get("env"));
    if let Some(extra) = account.get("env").and_then(|v| v.as_object()) {
        for (k, v) in extra {
            if let Some(s) = v.as_str() {
                env.insert(k.clone(), s.to_string());
            }
        }
    }
    let env_remove = string_list(account.get("env_remove"));

    let mcp_host = host::call_host(
        HostFn::ProviderGetMcpConfig,
        &json!({ "session_id": session_id }),
    )
    .unwrap_or(json!({}));
    let wiring = mcp_host
        .get("contents")
        .and_then(|v| v.as_str())
        .and_then(mcp::parse_contents);
    let extra_servers: Vec<String> = wiring
        .as_ref()
        .map(|w| w.extra_servers.iter().map(|(n, _)| n.clone()).collect())
        .unwrap_or_default();
    if let Some(w) = &wiring {
        env.insert(mcp::TOKEN_ENV_VAR.into(), w.token.clone());
        let _ = host::call_host(
            HostFn::ProviderWriteFile,
            &json!({
                "session_id": session_id,
                "path": ".mcp.json",
                "contents": mcp::workspace_mcp_json(w),
            }),
        );
    }

    let args = argv::build_cli_args(
        model,
        &prompt,
        conversation_id.as_deref(),
        cfg.get("effort").and_then(|v| v.as_str()),
        &system_prompt,
        wiring.is_some(),
        &extra_servers,
        &string_list(cfg.get("extra_disallowed_tools")),
        cfg.get("is_pre_hatcher")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    );

    emit(
        session_id,
        &ProviderEvent::Started {
            model: model.to_string(),
            conversation_id: conversation_id.clone(),
            metadata: json!({ "provider": "grok" }),
        },
    )?;

    host::call_host(
        HostFn::ProviderSpawn,
        &json!({
            "session_id": session_id,
            "command": "grok",
            "args": args,
            "env": env,
            "env_remove": env_remove,
            "cwd": working_dir,
        }),
    )?;

    let mut conv = conversation_id.clone();
    let mut usage = UsageTracker::default();
    let mut stream_error: Option<String> = None;
    let main_model = model.strip_prefix("grok:").unwrap_or(model);
    let main_model = main_model.split('@').next();

    loop {
        if should_stop(session_id) {
            let _ = kill(session_id);
        }
        let line = read_line(session_id, 200)?;
        if line
            .get("timeout")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        if line
            .get("stopped")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
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
            if let Some(err) = stream_error {
                emit(
                    session_id,
                    &ProviderEvent::Crashed {
                        reason: err,
                        error_kind: CrashKind::Unknown,
                        exit_code,
                        stderr,
                    },
                )?;
            } else if should_stop(session_id) {
                emit(
                    session_id,
                    &ProviderEvent::Crashed {
                        reason: "interrupted".into(),
                        error_kind: CrashKind::Interrupted,
                        exit_code,
                        stderr,
                    },
                )?;
            } else {
                emit(
                    session_id,
                    &ProviderEvent::Completed {
                        conversation_id: conv,
                        result_meta: Value::Null,
                    },
                )?;
            }
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
        if let Some(err) = parser::error_reason(&json_line) {
            stream_error = Some(err);
        }
        for ev in parser::parse_stream_json(&json_line, &mut conv, &mut usage, main_model) {
            emit(session_id, &ev)?;
        }
    }
}

fn emit(session_id: &str, event: &ProviderEvent) -> Result<(), String> {
    let value = serde_json::to_value(event).map_err(|e| e.to_string())?;
    host::call_host(
        HostFn::EmitProviderEvent,
        &json!({ "session_id": session_id, "event": value }),
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

fn string_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn map_env(v: Option<&Value>) -> std::collections::HashMap<String, String> {
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
