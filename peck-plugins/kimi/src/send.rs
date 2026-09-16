use serde_json::{Value, json};

use crate::argv;
use crate::event::{CrashKind, ProviderEvent};
use crate::host::{self, HostFn};
use crate::mcp;
use crate::parser;

/// Appended to a failed-to-spawn reason so the crash row tells the user how
/// to fix it.
const SPAWN_HINT: &str = "Install Kimi Code with `curl -fsSL \
     https://code.kimi.com/kimi-code/install.sh | bash` or point the \
     plugin's CLI Path setting at the binary.";

/// Crash reason for a child that exited without a successful result or
/// anything on stderr.
const EMPTY_EXIT_REASON: &str = "kimi exited without a successful result";

pub fn run(payload: &Value) -> Result<(), String> {
    let session_id = payload
        .get("session_id")
        .and_then(|v| v.as_str())
        .ok_or("provider.send payload missing session_id")?;
    let cfg = payload.get("spawn_config").cloned().unwrap_or(json!({}));
    let prompt = payload
        .pointer("/message/text")
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
    let raw_model = cfg
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // Strip `kimi:` / `@account` BEFORE the default/auto comparison: the
    // seed model is stored as `kimi:default`, and comparing the prefixed id
    // would substitute nothing and later send `--model default`.
    let stripped = raw_model.strip_prefix("kimi:").unwrap_or(&raw_model);
    let base_model = stripped.split('@').next().unwrap_or(stripped).to_string();
    let mut model = base_model;
    if model.is_empty() || model == "default" || model == "auto" {
        model = crate::settings::str("default_model")
            .filter(|m| m != "default" && m != "auto")
            .unwrap_or_default();
    }

    // The account env host fn needs the ORIGINAL id — the `@account` suffix
    // selects the credential to inject.
    let account = host::call_host(
        HostFn::ProviderAccountEnv,
        &json!({ "session_id": session_id, "model": raw_model }),
    )?;
    let mut env = map_env(cfg.get("env"));
    if let Some(key) = crate::settings::str("api_key") {
        env.entry("KIMI_API_KEY".into()).or_insert(key);
    }
    if let Some(url) = crate::settings::str("base_url") {
        env.entry("KIMI_BASE_URL".into()).or_insert(url);
    }
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
    if let Some(w) = mcp_host
        .get("contents")
        .and_then(|v| v.as_str())
        .and_then(mcp::parse_contents)
    {
        // Merge into the existing project file, preserving user servers and
        // unrelated keys; an invalid file is left untouched (and MCP skipped
        // for the turn), a no-op merge skips the write.
        let existing = read_workspace_file(session_id, ".kimi-code/mcp.json");
        if let Ok(next) =
            mcp::merge_workspace_mcp_json(existing.as_deref(), &w.url, &w.extra_servers)
        {
            if let Some(text) = next {
                write_workspace_file(session_id, ".kimi-code/mcp.json", &text);
            }
            env.insert(mcp::TOKEN_ENV_VAR.into(), w.token.clone());
        }
    }

    let cli_model = if model.is_empty() {
        None
    } else {
        Some(model.as_str())
    };
    let args = argv::build_cli_args(
        cli_model,
        &prompt,
        conversation_id.as_deref(),
        &system_prompt,
    );

    // `kimi --help` carries no image/attachment flag, so say so in the
    // transcript rather than silently answering the text alone.
    let attachments = payload
        .pointer("/message/attachments")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    if attachments > 0 {
        let _ = emit(
            session_id,
            &ProviderEvent::Text {
                text: attachments_dropped_note("kimi", attachments),
            },
        );
    }

    emit(
        session_id,
        &ProviderEvent::Started {
            model: raw_model.clone(),
            conversation_id: conversation_id.clone(),
            metadata: json!({ "provider": "kimi" }),
        },
    )?;

    host::call_host(
        HostFn::ProviderSpawn,
        &json!({
            "session_id": session_id,
            "command": crate::settings::resolve_kimi_fallback(crate::settings::cli_path("kimi")),
            "args": args,
            "env": env,
            "env_remove": env_remove,
            "cwd": working_dir,
        }),
    )
    .map_err(|e| format!("{e}. {SPAWN_HINT}"))?;

    let mut conv = conversation_id;
    // Set once the trailing `session.resume_hint` meta frame arrives — the
    // signal that the turn actually finished with a result. EOF without it
    // is a crash, not a completion (mirrors the old harness's
    // `success_on_output: false`).
    let mut saw_result = false;
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
        if json_line.get("type").and_then(|v| v.as_str()) == Some("session.resume_hint") {
            saw_result = true;
        }
        for ev in parser::parse_stream_json(&json_line, &mut conv) {
            emit(session_id, &ev)?;
        }
    }
}

/// Classify a child that hit EOF with no successful result. Kimi fails fast
/// with "No model configured" on stderr when unsigned-in; that maps to
/// `AuthExpired` with a re-login hint. A silent exit gets the empty-exit
/// reason.
fn classify_failed_exit(stderr: &str) -> (String, CrashKind) {
    if stderr.contains("No model configured") {
        (
            "Kimi Code isn't signed in on this host. Run `kimi login` (or add a \
             provider to ~/.kimi-code/config.toml / set an API key in the plugin \
             settings), then try again."
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
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn map_env(v: Option<&Value>) -> std::collections::HashMap<String, String> {
    v.and_then(|v| v.as_object())
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// EOF with nothing to show for it must classify, not complete:
    /// "No model configured" maps to `AuthExpired` with the `kimi login`
    /// hint, silence to the empty-exit reason, other stderr verbatim.
    #[test]
    fn failed_exit_classification_matches_the_old_markers() {
        let (reason, kind) =
            classify_failed_exit("No model configured. Run `kimi` and use /login to sign in");
        assert_eq!(kind, CrashKind::AuthExpired);
        assert!(reason.contains("kimi login"));

        let (reason, kind) = classify_failed_exit("");
        assert_eq!(kind, CrashKind::NoOutput);
        assert_eq!(reason, EMPTY_EXIT_REASON);

        let (reason, kind) = classify_failed_exit("boom\n");
        assert_eq!(kind, CrashKind::Unknown);
        assert_eq!(reason, "boom");
    }
}
