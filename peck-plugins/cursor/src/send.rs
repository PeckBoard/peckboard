//! One-shot `cursor-agent` turn.

use serde_json::{Value, json};

use crate::argv;
use crate::event::{CrashKind, ProviderEvent};
use crate::host::{self, HostFn};
use crate::mcp;
use crate::parser::{self, TurnState};

/// Crash reason for a child that exited without streaming anything.
const EMPTY_EXIT_REASON: &str = "cursor-agent exited without output";

/// Session-folder directory image attachments are staged into so
/// `--image PATH` can read them.
const IMAGE_DIR: &str = ".peckboard-cursor-images";

/// The prompt for a message that is only images: `--print` needs some text.
const IMAGE_ONLY_PROMPT: &str = "See the attached image(s).";
/// The project MCP config cursor-agent reads, relative to the project root.
const MCP_JSON: &str = ".cursor/mcp.json";

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
        // The token env must match the turn's spawn env — cursor-agent
        // hashes the env-interpolated server config when recording the
        // approval.
        env.insert(mcp::TOKEN_ENV_VAR.into(), w.token.clone());
        let notes = match wire_workspace_mcp(session_id, w) {
            Ok(names) => approve_servers(&cli, &names, &env, &env_remove, &working_dir),
            Err(e) => vec![format!(
                "**Note:** MCP servers won't load this turn, including Peckboard's tools: {e}"
            )],
        };
        for text in notes {
            let _ = emit(session_id, &ProviderEvent::Text { text });
        }
    }
    // Image attachments are staged into the session folder for `--image`;
    // anything else is dropped with a transcript notice.
    let attachments = message
        .get("attachments")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let (image_paths, dropped) = stage_image_attachments(session_id, &attachments);
    let prompt = if prompt.trim().is_empty() && !image_paths.is_empty() {
        IMAGE_ONLY_PROMPT.to_string()
    } else {
        prompt
    };
    let args = argv::build_cli_args(
        &model,
        &prompt,
        conversation_id.as_deref(),
        &system_prompt,
        crate::settings::bool("auto_approve", true),
        &image_paths,
    );

    if dropped > 0 {
        let _ = emit(
            session_id,
            &ProviderEvent::Text {
                text: attachments_dropped_note(dropped),
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
    } else if tail.contains("unknown option '--image'") {
        // `--image` is hidden and marked internal in the CLI, so an update
        // can remove it. Name that instead of a bare parser error.
        (
            "This cursor-agent version no longer accepts the `--image` flag Peckboard uses to \
             send images. Resend without the image, or switch the session to another provider."
                .into(),
            CrashKind::Unknown,
        )
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
fn approve_servers(
    cli: &str,
    names: &[String],
    env: &std::collections::HashMap<String, String>,
    env_remove: &[String],
    working_dir: &str,
) -> Vec<String> {
    let mut notes = Vec::new();
    for name in names {
        // The turn's exact env: the approval is recorded against the
        // env-INTERPOLATED server config, so `enable` must resolve the same
        // values as the turn, and an account's config dir must match too.
        let result = host::call_host(
            HostFn::ProviderProbe,
            &json!({
                "command": cli,
                "args": ["mcp", "enable", name],
                "env": env,
                "env_remove": env_remove,
                // Approval is workspace-scoped.
                "cwd": working_dir,
                "timeout_ms": 20_000,
            }),
        );
        if let Some(detail) = approval_failure(&result) {
            notes.push(format!(
                "**Note:** MCP server `{name}` couldn't be approved, so its tools won't load \
                 this turn: {detail}"
            ));
        }
    }
    notes
}

/// Why an `mcp enable` probe failed; `None` when it succeeded.
fn approval_failure(result: &Result<Value, String>) -> Option<String> {
    let v = match result {
        Ok(v) => v,
        Err(e) => return Some(e.clone()),
    };
    if v.get("exit_code").and_then(|c| c.as_i64()) == Some(0) {
        return None;
    }
    let text = |key: &str| v.get(key).and_then(|s| s.as_str()).unwrap_or("").trim();
    let detail = [text("stderr"), text("stdout")]
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or("`cursor-agent mcp enable` failed");
    Some(detail.to_string())
}

/// Merge Peckboard's servers into the `.cursor/mcp.json` the CLI actually
/// reads, and return every server configured there — Peckboard's and the
/// ones the project added itself.
fn wire_workspace_mcp(session_id: &str, wiring: &mcp::McpWiring) -> Result<Vec<String>, String> {
    let existing = read_workspace_mcp_json(session_id)?;
    let text = match mcp::merge_workspace_mcp_json(existing.as_deref(), wiring)? {
        Some(text) => {
            write_workspace_mcp_json(session_id, &text)?;
            text
        }
        None => existing.unwrap_or_default(),
    };
    Ok(mcp::configured_server_names(&text))
}

/// The transcript note for attachments that couldn't be sent: non-image
/// files, or images the host couldn't stage.
fn attachments_dropped_note(count: usize) -> String {
    let plural = if count == 1 { "" } else { "s" };
    format!(
        "**Note:** {count} attachment{plural} on your message {} not sent — \
         `cursor-agent` only takes images, up to 20 MiB each. For other files, \
         save them into the working directory and reference them by path.",
        if count == 1 { "was" } else { "were" }
    )
}

fn is_image_attachment(att: &Value) -> bool {
    let mime = att.get("mime_type").and_then(|v| v.as_str()).unwrap_or("");
    if mime.to_ascii_lowercase().starts_with("image/") {
        return true;
    }
    let name = att.get("filename").and_then(|v| v.as_str()).unwrap_or("");
    let ext = std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp")
}

/// Stage image attachments into `IMAGE_DIR` under the session folder via
/// the host, so `cursor-agent --image PATH` can read them. Returns (staged
/// paths, dropped count).
fn stage_image_attachments(session_id: &str, attachments: &[Value]) -> (Vec<String>, usize) {
    let mut paths = Vec::new();
    let mut dropped = 0usize;
    for (i, att) in attachments.iter().enumerate() {
        let data = att
            .get("data_base64")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !is_image_attachment(att) || data.is_empty() {
            dropped += 1;
            continue;
        }
        let name = att
            .get("filename")
            .and_then(|v| v.as_str())
            .unwrap_or("image");
        let rel = format!("{IMAGE_DIR}/{}", safe_filename(name, i));
        match host::call_host(
            HostFn::ProviderWriteFile,
            &json!({
                "session_id": session_id,
                "path": rel,
                "contents_base64": data,
            }),
        ) {
            Ok(v) => paths.push(
                v.get("path")
                    .and_then(|p| p.as_str())
                    .map(str::to_string)
                    .unwrap_or(rel),
            ),
            Err(_) => dropped += 1,
        }
    }
    (paths, dropped)
}

/// A path-safe staged name; the index keeps two same-named images apart.
fn safe_filename(name: &str, idx: usize) -> String {
    let base = std::path::Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("image");
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{idx}-{cleaned}")
}

/// `.cursor/mcp.json` at the project root, where cursor-agent looks: the
/// nearest ancestor with a `.git`, so a session folder below a repo's root
/// shares the root's file. A read failure is an error, never "absent": merging
/// into nothing would overwrite a file that does exist.
fn read_workspace_mcp_json(session_id: &str) -> Result<Option<String>, String> {
    let v = host::call_host(
        HostFn::ProviderReadFile,
        &json!({ "session_id": session_id, "path": MCP_JSON, "project_root": true }),
    )?;
    if !v.get("exists").and_then(|e| e.as_bool()).unwrap_or(false) {
        return Ok(None);
    }
    Ok(v.get("contents")
        .and_then(|c| c.as_str())
        .map(str::to_string))
}

fn write_workspace_mcp_json(session_id: &str, contents: &str) -> Result<(), String> {
    host::call_host(
        HostFn::ProviderWriteFile,
        &json!({
            "session_id": session_id,
            "path": MCP_JSON,
            "contents": contents,
            "project_root": true,
        }),
    )
    .map(|_| ())
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
        assert!(attachments_dropped_note(1).contains("1 attachment on your message was"));
        assert!(attachments_dropped_note(3).contains("3 attachments on your message were"));
    }

    #[test]
    fn images_are_detected_by_mime_or_filename() {
        assert!(is_image_attachment(&json!({"mime_type": "image/png"})));
        assert!(is_image_attachment(&json!({"filename": "a.JPG"})));
        assert!(!is_image_attachment(
            &json!({"mime_type": "application/pdf", "filename": "a.pdf"})
        ));
    }

    #[test]
    fn staged_names_cannot_escape_the_image_dir() {
        assert_eq!(safe_filename("../../etc/my pic.png", 2), "2-my_pic.png");
    }

    #[test]
    fn a_removed_image_flag_gets_a_clear_reason() {
        let (reason, _) = classify_failed_exit("error: unknown option '--image'\n");
        assert!(reason.contains("no longer accepts"), "{reason}");
    }

    #[test]
    fn approval_failures_name_the_reason() {
        assert_eq!(approval_failure(&Ok(json!({"exit_code": 0}))), None);
        assert_eq!(
            approval_failure(&Ok(json!({"exit_code": 1, "stderr": " nope \n"}))).as_deref(),
            Some("nope")
        );
        assert_eq!(
            approval_failure(&Err("probe timed out".into())).as_deref(),
            Some("probe timed out")
        );
    }
}
