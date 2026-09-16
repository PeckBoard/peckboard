//! One-shot `codex exec` turn.

use serde_json::{Value, json};

use crate::argv;
use crate::event::{CrashKind, ProviderEvent};
use crate::host::{self, HostFn};
use crate::mcp;
use crate::parser::{self, TurnState};

/// Appended to a failed-to-spawn reason so the crash row tells the user how
/// to fix it.
const SPAWN_HINT: &str = "Install the Codex CLI with `curl -fsSL https://chatgpt.com/codex/install.sh | sh`, \
     or point the plugin's CLI Path setting at the binary. Docs: \
     https://learn.chatgpt.com/docs/codex/cli";

/// Crash reason for a child that exited without an error frame, a
/// `turn.completed`, or anything on stderr.
const EMPTY_EXIT_REASON: &str = "codex exited without a successful result";

const AUTH_HINT: &str = "Codex isn't signed in. Sign in with ChatGPT in Settings → Codex \
                         Accounts, or run `codex login --device-auth` on the host.";

/// Stderr fragments that mean the CLI wants a (re-)login.
const AUTH_MARKERS: &[&str] = &[
    "Not logged in",
    "Not signed in",
    "no Codex credentials were found",
    "CODEX_API_KEY",
];

/// Session-folder directory image attachments are staged into so
/// `--image PATH` can see them.
const IMAGE_DIR: &str = ".peckboard-codex-images";

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
    // The host names the vars to strip only when an account credential is
    // injected (a ChatGPT sign-in must not be stolen onto API-key billing);
    // a host-credential spawn keeps the user's own CODEX_API_KEY working.
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
    let overrides = wiring
        .as_ref()
        .map(mcp::cli_config_overrides)
        .unwrap_or_default();
    if let Some(w) = &wiring {
        env.insert(mcp::TOKEN_ENV_VAR.into(), w.token.clone());
    }

    // Image attachments are staged into the session folder for `--image`;
    // anything else is dropped with a transcript notice.
    let attachments = message
        .get("attachments")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let (image_paths, dropped) = stage_image_attachments(session_id, &attachments);

    let args = argv::build_cli_args(
        model,
        &prompt,
        conversation_id.as_deref(),
        cfg.get("effort").and_then(|v| v.as_str()),
        &system_prompt,
        &image_paths,
        &overrides,
    );

    if dropped > 0 {
        let _ = emit(
            session_id,
            &ProviderEvent::Text {
                text: attachments_dropped_note("codex", dropped),
            },
        );
    }

    emit(
        session_id,
        &ProviderEvent::Started {
            model: model.to_string(),
            conversation_id: conversation_id.clone(),
            metadata: json!({ "provider": "codex" }),
        },
    )?;

    host::call_host(
        HostFn::ProviderSpawn,
        &json!({
            "session_id": session_id,
            "command": crate::settings::cli_path("codex"),
            "args": args,
            "env": env,
            "env_remove": env_remove,
            "cwd": working_dir,
        }),
    )
    .map_err(|e| format!("{e}. {SPAWN_HINT}"))?;

    let mut state = TurnState::default();
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
            if let Some(err) = state.error.clone() {
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
            } else if state.completed || exit_code == Some(0) {
                emit(
                    session_id,
                    &ProviderEvent::Completed {
                        conversation_id: state.conversation_id.clone(),
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
        for ev in parser::parse_stream_json(&json_line, &mut state, Some(model)) {
            emit(session_id, &ev)?;
        }
    }
}

/// Classify a child that hit EOF with no error frame and no successful
/// result: the CLI's sign-in markers map to `AuthExpired` with a re-login
/// hint, a silent exit gets the empty-exit reason, and any other stderr
/// tail is surfaced verbatim.
fn classify_failed_exit(stderr: &str) -> (String, CrashKind) {
    let tail = stderr.trim();
    if AUTH_MARKERS.iter().any(|m| tail.contains(m)) {
        (AUTH_HINT.into(), CrashKind::AuthExpired)
    } else if tail.is_empty() {
        (EMPTY_EXIT_REASON.into(), CrashKind::NoOutput)
    } else {
        (tail.to_string(), CrashKind::Unknown)
    }
}

/// The transcript note for attachments codex can't take (non-image files,
/// or images the host couldn't stage).
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

fn is_image_attachment(att: &Value) -> bool {
    let mime = att.get("mime_type").and_then(|v| v.as_str()).unwrap_or("");
    if mime.to_ascii_lowercase().starts_with("image/") {
        return true;
    }
    let name = att.get("filename").and_then(|v| v.as_str()).unwrap_or("");
    mime_from_filename(name).starts_with("image/")
}

fn mime_from_filename(name: &str) -> &'static str {
    let ext = std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

/// Stage image attachments into `IMAGE_DIR` under the session folder via
/// the host, so `codex exec --image PATH` can read them. Returns (staged
/// paths, dropped count); non-image attachments and failed writes are
/// dropped and reported via [`attachments_dropped_note`].
fn stage_image_attachments(session_id: &str, attachments: &[Value]) -> (Vec<String>, usize) {
    if attachments.is_empty() {
        return (Vec::new(), 0);
    }
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
        // Raw image bytes ride as `contents_base64`; a host without base64
        // write support rejects the call and the attachment is reported
        // dropped instead of a corrupt file being staged.
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
    let cleaned = if cleaned.is_empty() {
        "image".to_string()
    } else {
        cleaned
    };
    format!("{idx}-{cleaned}")
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
    fn failed_exit_classification_matches_the_old_markers() {
        for marker in [
            "Not logged in",
            "Not signed in",
            "no Codex credentials were found",
            "CODEX_API_KEY is set but empty",
        ] {
            let (reason, kind) = classify_failed_exit(marker);
            assert_eq!(kind, CrashKind::AuthExpired, "marker: {marker}");
            assert_eq!(reason, AUTH_HINT);
        }

        let (reason, kind) = classify_failed_exit("  ");
        assert_eq!(kind, CrashKind::NoOutput);
        assert_eq!(reason, EMPTY_EXIT_REASON);

        let (reason, kind) = classify_failed_exit("segfault\n");
        assert_eq!(kind, CrashKind::Unknown);
        assert_eq!(reason, "segfault");
    }

    #[test]
    fn image_attachments_are_detected_by_mime_or_filename() {
        let by_mime = serde_json::json!({"filename":"x.bin","mime_type":"image/png"});
        assert!(is_image_attachment(&by_mime));
        let by_name = serde_json::json!({"filename":"shot.PNG","mime_type":""});
        assert!(is_image_attachment(&by_name));
        let neither = serde_json::json!({"filename":"notes.txt","mime_type":"text/plain"});
        assert!(!is_image_attachment(&neither));
    }

    #[test]
    fn staged_paths_are_indexed_and_sanitized_under_the_image_dir() {
        assert_eq!(safe_filename("a b?.png", 0), "0-a_b_.png");
        assert_eq!(safe_filename("../../etc/passwd", 1), "1-passwd");
        assert_eq!(safe_filename("", 2), "2-image");
        let rel = format!("{IMAGE_DIR}/{}", safe_filename("shot.png", 0));
        assert_eq!(rel, ".peckboard-codex-images/0-shot.png");
    }

    #[test]
    fn attachments_note_pluralizes() {
        assert!(attachments_dropped_note("codex", 1).contains("1 attachment on your message was"));
        assert!(
            attachments_dropped_note("codex", 2).contains("2 attachments on your message were")
        );
    }
}
