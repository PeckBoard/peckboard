//! One-shot grok CLI turn.

use serde_json::{Value, json};

use crate::argv;
use crate::event::{CrashKind, ProviderEvent};
use crate::host::{self, HostFn};
use crate::mcp;
use crate::parser::{self, UsageTracker};

/// Appended to a failed-to-spawn reason so the crash row tells the user how
/// to fix it.
const SPAWN_HINT: &str =
    "Install the Grok CLI, or point the plugin's CLI Path setting at the binary.";

/// Crash reason for a child that exited without an error frame, a successful
/// result, or anything on stderr.
const EMPTY_EXIT_REASON: &str = "grok exited without a successful result";

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
    let contents = mcp_host
        .get("contents")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let wiring = contents.as_deref().and_then(mcp::parse_contents);
    // A config without the peckboard entry still contributes its
    // user-defined servers.
    let extras: Vec<(String, Value)> = match &wiring {
        Some(w) => w.extra_servers.clone(),
        None => contents
            .as_deref()
            .map(mcp::extra_servers_from_contents)
            .unwrap_or_default(),
    };
    let extra_servers: Vec<String> = extras.iter().map(|(n, _)| n.clone()).collect();
    if let Some(w) = &wiring {
        env.insert(mcp::TOKEN_ENV_VAR.into(), w.token.clone());
    }
    let peckboard_url = wiring.as_ref().map(|w| w.url.as_str());
    // `.mcp.json` (Claude-compat): merge into the existing file, preserving
    // user servers and unrelated keys; an invalid file is left untouched and
    // a no-op merge skips the write. Best-effort: the turn runs without MCP
    // on any failure.
    let existing = read_workspace_file(session_id, ".mcp.json");
    if let Ok(Some(text)) =
        mcp::merge_workspace_mcp_json(existing.as_deref(), peckboard_url, &extras)
    {
        write_workspace_file(session_id, ".mcp.json", &text);
    }
    // `.grok/config.toml` (native): grok always scans it, while `.mcp.json`
    // is skipped once the Claude import prompt is dismissed. Managed-block
    // markers keep hand-written TOML untouched.
    let existing_toml = read_workspace_file(session_id, ".grok/config.toml").unwrap_or_default();
    if let Some(text) = mcp::merge_workspace_grok_toml(&existing_toml, peckboard_url, &extras) {
        write_workspace_file(session_id, ".grok/config.toml", &text);
    }

    let args = argv::build_cli_args(
        model,
        &prompt,
        conversation_id.as_deref(),
        cfg.get("effort").and_then(|v| v.as_str()),
        &system_prompt,
        contents.is_some(),
        &extra_servers,
        &string_list(cfg.get("extra_disallowed_tools")),
        cfg.get("is_pre_hatcher")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    );

    // `grok --help` carries no image/attachment flag, so say so in the
    // transcript rather than silently answering the text alone.
    let attachments = message
        .get("attachments")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    if attachments > 0 {
        let _ = emit(
            session_id,
            &ProviderEvent::Text {
                text: attachments_dropped_note("grok", attachments),
            },
        );
    }

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
            "command": crate::settings::cli_path("grok"),
            "args": args,
            "env": env,
            "env_remove": env_remove,
            "cwd": working_dir,
        }),
    )
    .map_err(|e| format!("{e}. {SPAWN_HINT}"))?;
    let mut conv = conversation_id;
    let mut usage = UsageTracker::default();
    let mut stream_error: Option<String> = None;
    // Set once the parser sees grok's terminal `end` frame with no error —
    // the signal that the turn actually produced a result. EOF without it is
    // a crash, not a completion (mirrors the old harness's
    // `success_on_output: false`).
    let mut saw_result = false;
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
            } else if saw_result || exit_code == Some(0) {
                emit(
                    session_id,
                    &ProviderEvent::Completed {
                        conversation_id: conv,
                        result_meta: Value::Null,
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
        if let Some(err) = parser::error_reason(&json_line) {
            stream_error = Some(err);
        } else if json_line.get("type").and_then(|v| v.as_str()) == Some("end") {
            saw_result = true;
        }
        for ev in parser::parse_stream_json(&json_line, &mut conv, &mut usage, main_model) {
            emit(session_id, &ev)?;
        }
    }
}

/// Classify a child that hit EOF with no error frame and no successful
/// result. The sign-in markers grok prints to stderr map to `AuthExpired`
/// with a re-login hint; a silent exit gets the empty-exit reason.
fn classify_failed_exit(stderr: &str) -> (String, CrashKind) {
    if stderr.contains("accounts.x.ai/oauth2/device") {
        (
            "This Grok account isn't signed in. Open Settings → Grok accounts and \
             complete the browser sign-in, then try again."
                .into(),
            CrashKind::AuthExpired,
        )
    } else if stderr.contains("Not signed in") {
        (
            "This Grok account isn't signed in. Open Settings → Grok accounts and \
             complete the sign-in, then try again."
                .into(),
            CrashKind::AuthExpired,
        )
    } else if stderr.trim().is_empty() {
        (EMPTY_EXIT_REASON.into(), CrashKind::NoOutput)
    } else {
        (stderr.trim().to_string(), CrashKind::Unknown)
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

fn emit(session_id: &str, event: &ProviderEvent) -> Result<(), String> {
    let value = serde_json::to_value(event).map_err(|e| e.to_string())?;
    host::call_host(
        HostFn::EmitProviderEvent,
        &json!({ "session_id": session_id, "event": value }),
    )
    .map(|_| ())
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

    /// EOF with nothing to show for it must classify, not complete: the
    /// sign-in markers map to `AuthExpired`, silence to the empty-exit
    /// reason, and any other stderr tail is surfaced verbatim.
    #[test]
    fn failed_exit_classification_matches_the_old_markers() {
        let (reason, kind) =
            classify_failed_exit("Visit https://accounts.x.ai/oauth2/device to sign in");
        assert_eq!(kind, CrashKind::AuthExpired);
        assert!(reason.contains("browser sign-in"));

        let (reason, kind) = classify_failed_exit("Error: Not signed in.");
        assert_eq!(kind, CrashKind::AuthExpired);
        assert!(reason.contains("complete the sign-in"));

        let (reason, kind) = classify_failed_exit("  ");
        assert_eq!(kind, CrashKind::NoOutput);
        assert_eq!(reason, EMPTY_EXIT_REASON);

        let (reason, kind) = classify_failed_exit("segfault\n");
        assert_eq!(kind, CrashKind::Unknown);
        assert_eq!(reason, "segfault");
    }

    #[test]
    fn attachments_note_pluralizes() {
        assert!(attachments_dropped_note("grok", 1).contains("1 attachment on your message was"));
        assert!(attachments_dropped_note("grok", 2).contains("2 attachments on your message were"));
    }
}
