//! One-shot `cursor-agent` turn.

use serde_json::{Value, json};

use crate::argv;
use crate::event::{CrashKind, ProviderEvent};
use crate::host::{self, HostFn};
use crate::mcp;
use crate::parser::{self, TurnState};

/// Crash reason for a child that exited without streaming anything.
const EMPTY_EXIT_REASON: &str = "cursor-agent exited without output";

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
    let model = effective_model(
        cfg.get("model").and_then(|v| v.as_str()).unwrap_or(""),
        crate::settings::str("default_model"),
    );
    let cli = crate::settings::cli_path("cursor-agent");

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
    if let Some(w) = &wiring {
        // Merge into the workspace `.cursor/mcp.json`, preserving user
        // servers and unrelated keys; a file we can't merge (invalid JSON)
        // is left untouched and the turn runs without MCP wiring rather
        // than clobbering it.
        let existing = read_workspace_file(session_id, ".cursor/mcp.json");
        let wired = match mcp::merge_workspace_mcp_json(existing.as_deref(), w) {
            Ok(Some(text)) => {
                write_workspace_file(session_id, ".cursor/mcp.json", &text);
                true
            }
            Ok(None) => true,
            Err(_) => false,
        };
        if wired {
            // The token env must match the turn's spawn env — cursor-agent
            // hashes the env-interpolated server config when recording the
            // approval.
            env.insert(mcp::TOKEN_ENV_VAR.into(), w.token.clone());
            approve_servers(&cli, w, &working_dir);
        }
    }
    let args = argv::build_cli_args(
        &model,
        &prompt,
        conversation_id.as_deref(),
        &system_prompt,
        crate::settings::bool("auto_approve", true),
    );

    // `cursor-agent --help` carries no image/attachment flag, so say so in
    // the transcript rather than silently answering the text alone.
    let attachments = message
        .get("attachments")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    if attachments > 0 {
        let _ = emit(
            session_id,
            &ProviderEvent::Text {
                text: attachments_dropped_note("cursor-agent", attachments),
            },
        );
    }

    host::call_host(
        HostFn::ProviderSpawn,
        &json!({
            "session_id": session_id,
            "command": cli,
            "args": args,
            "env": env,
            "env_remove": env_remove,
            "cwd": working_dir,
        }),
    )?;

    let mut state = TurnState::default();
    // cursor-agent exits non-zero on some turns that in fact streamed a
    // complete answer, so any real output counts as success (the old
    // harness's `success_on_output: true`). The parser's synthetic
    // `Started` doesn't count — the CLI prints its init frame before
    // failing turns too.
    let mut saw_output = false;
    loop {
        if should_stop(session_id) {
            let _ = kill(session_id);
        }
        let line = read_line(session_id, 200)?;
        if line
            .get("timeout")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            || line
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
            let signal = line
                .get("signal")
                .and_then(|v| v.as_i64())
                .map(|n| n as i32);
            if should_stop(session_id) {
                emit(
                    session_id,
                    &ProviderEvent::Crashed {
                        reason: "interrupted".into(),
                        error_kind: CrashKind::Interrupted,
                        exit_code,
                        stderr,
                    },
                )?;
            } else if exit_code == Some(0) || saw_output {
                emit(
                    session_id,
                    &ProviderEvent::Completed {
                        conversation_id: state.conversation_id.clone(),
                        result_meta: Value::Null,
                    },
                )?;
            } else if let Some(sig) = signal {
                emit(
                    session_id,
                    &ProviderEvent::Crashed {
                        reason: killed_by_signal_reason("cursor-agent", sig),
                        error_kind: CrashKind::ExitedMidTurn,
                        exit_code,
                        stderr,
                    },
                )?;
            } else {
                let (reason, error_kind) = classify_failed_exit(stderr.as_deref().unwrap_or(""));
                emit(
                    session_id,
                    &ProviderEvent::Crashed {
                        reason,
                        error_kind,
                        exit_code,
                        stderr,
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
        for ev in parser::parse_stream_json(&json_line, &mut state) {
            saw_output |= !matches!(ev, ProviderEvent::Started { .. });
            emit(session_id, &ev)?;
        }
    }
}

/// The model actually passed to the CLI: strip our `cursor:` prefix; an
/// empty leftover or a foreign provider prefix falls back to the configured
/// default (the old `resolve_model` breadth), while a bare id is passed
/// through so catalog entries stored without a prefix keep working.
/// Nothing usable anywhere means "auto" — let Cursor choose.
fn effective_model(raw: &str, default_model: Option<String>) -> String {
    let raw = raw.trim();
    let resolved = if raw.is_empty() || raw == "default" {
        None
    } else if let Some(rest) = raw.strip_prefix("cursor:") {
        Some(rest.to_string()).filter(|r| !r.is_empty())
    } else if raw.contains(':') {
        // Another provider's prefix — never forward it to cursor-agent.
        None
    } else {
        Some(raw.to_string())
    };
    resolved
        .or(default_model)
        .unwrap_or_else(|| "auto".to_string())
}

/// Classify a child that hit EOF with no output and a non-zero exit. An
/// unauthenticated cursor-agent exits non-zero without an interactive
/// prompt, so there are no stderr markers to match — a silent exit gets the
/// empty-exit reason and any stderr tail is surfaced verbatim.
fn classify_failed_exit(stderr: &str) -> (String, CrashKind) {
    let tail = stderr.trim();
    if tail.is_empty() {
        (EMPTY_EXIT_REASON.into(), CrashKind::NoOutput)
    } else {
        (tail.to_string(), CrashKind::Unknown)
    }
}

/// Reason for a child that died to a signal instead of exiting: the OS or
/// service manager killed it mid-turn — the CLI itself never got to report
/// anything, so "exited without a result" would point at the wrong culprit.
fn killed_by_signal_reason(provider: &str, signal: i32) -> String {
    let name = match signal {
        6 => " (SIGABRT)",
        9 => " (SIGKILL)",
        11 => " (SIGSEGV)",
        15 => " (SIGTERM)",
        _ => "",
    };
    format!(
        "{provider} was killed by signal {signal}{name} before finishing — \
         the OS or service manager took it down mid-turn (usually the \
         out-of-memory killer; check `journalctl -k` for oom-kill events). \
         Send the message again to retry."
    )
}

/// Best-effort per-server approval (`mcp enable` is also the approval
/// verb): sticky per server config, reset when the entry changes, so it is
/// re-asserted before every turn. Failures are ignored — worst case the
/// agent runs without that server's tools, same as before wiring. The
/// host's 60 s probe cache makes per-turn re-runs cheap.
fn approve_servers(cli: &str, wiring: &mcp::McpWiring, working_dir: &str) {
    let mut probe_env = serde_json::Map::new();
    // The approval is recorded against the env-INTERPOLATED server config,
    // so `enable` must resolve the exact same header value as the turn.
    probe_env.insert(mcp::TOKEN_ENV_VAR.to_string(), json!(wiring.token));
    for name in mcp::approval_server_names(wiring) {
        let _ = host::call_host(
            HostFn::ProviderProbe,
            &json!({
                "command": cli,
                "args": ["mcp", "enable", name],
                "env": probe_env,
                // Approval is workspace-scoped; hosts that support a probe
                // cwd honour it, older ones ignore the field.
                "cwd": working_dir,
                "timeout_ms": 10_000,
            }),
        );
    }
}

/// The transcript note for attachments a text-only CLI can't take.
fn attachments_dropped_note(provider: &str, count: usize) -> String {
    let plural = if count == 1 { "" } else { "s" };
    format!(
        "**Note:** {count} attachment{plural} on your message {} not sent — \
         the `{provider}` CLI accepts text only. Save the file into the \
         working directory and reference it by path, or switch the session \
         to a provider with image support (Claude, Ollama).",
        if count == 1 { "was" } else { "were" }
    )
}

/// Read a session-folder file via the host; `None` when absent or on any
/// host error (treated as a missing file, like the old `read_to_string().ok()`).
fn read_workspace_file(session_id: &str, path: &str) -> Option<String> {
    let v = host::call_host(
        HostFn::ProviderReadFile,
        &json!({ "session_id": session_id, "path": path }),
    )
    .ok()?;
    if !v.get("exists").and_then(|e| e.as_bool()).unwrap_or(false) {
        return None;
    }
    v.get("contents")
        .and_then(|c| c.as_str())
        .map(str::to_string)
}

fn write_workspace_file(session_id: &str, path: &str, contents: &str) {
    let _ = host::call_host(
        HostFn::ProviderWriteFile,
        &json!({ "session_id": session_id, "path": path, "contents": contents }),
    );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_model_applies_default_breadth() {
        let default = || Some("composer-2.5".to_string());
        // Our prefix strips cleanly.
        assert_eq!(effective_model("cursor:gpt-5", default()), "gpt-5");
        // Bare catalog ids pass through.
        assert_eq!(effective_model("gpt-5", default()), "gpt-5");
        // Nothing usable → configured default.
        assert_eq!(effective_model("", default()), "composer-2.5");
        assert_eq!(effective_model("default", default()), "composer-2.5");
        assert_eq!(effective_model("cursor:", default()), "composer-2.5");
        // A foreign provider prefix must never reach cursor-agent.
        assert_eq!(effective_model("claude:opus", default()), "composer-2.5");
        // No default configured → let Cursor choose.
        assert_eq!(effective_model("claude:opus", None), "auto");
        assert_eq!(effective_model("", None), "auto");
    }

    #[test]
    fn failed_exit_classification() {
        let (reason, kind) = classify_failed_exit("  ");
        assert_eq!(kind, CrashKind::NoOutput);
        assert_eq!(reason, EMPTY_EXIT_REASON);
        let (reason, kind) = classify_failed_exit("boom\n");
        assert_eq!(kind, CrashKind::Unknown);
        assert_eq!(reason, "boom");
    }

    #[test]
    fn signal_kill_reason_names_the_signal() {
        let reason = killed_by_signal_reason("cursor-agent", 9);
        assert!(reason.contains("killed by signal 9 (SIGKILL)"), "{reason}");
    }

    #[test]
    fn attachments_note_pluralizes() {
        assert!(
            attachments_dropped_note("cursor-agent", 1)
                .contains("1 attachment on your message was")
        );
        assert!(
            attachments_dropped_note("cursor-agent", 3)
                .contains("3 attachments on your message were")
        );
    }
}
