//! Browser "log in with Cursor" — the in-app equivalent of running
//! `cursor-agent login` in a terminal.
//!
//! Like the Grok flow, this is driven by the CLI: with `NO_OPEN_BROWSER` set it
//! prints a `https://cursor.com/loginDeepControl?…` link, then blocks until the
//! user authorises in the browser. So [`CursorLoginManager::start`] spawns it,
//! scrapes the link, and leaves it running in the background.
//!
//! Cursor has no per-account home the way the other CLIs do, so this is the
//! host's one login. [`signed_in`] asks the CLI rather than inspecting its
//! credential files, whose location and format are Cursor's to change.

use std::process::Stdio;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use tokio::sync::{Mutex, Notify};

use super::codex_login::{spawn_line_forwarder, strip_ansi};

/// How long we wait for the CLI to print the link before giving up.
const URL_TIMEOUT: Duration = Duration::from_secs(30);
/// Overall lifetime of a login attempt.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);
/// `cursor-agent status` is polled while a sign-in dialog is open.
const STATUS_TIMEOUT: Duration = Duration::from_secs(15);

pub static CURSOR_LOGIN: LazyLock<CursorLoginManager> = LazyLock::new(CursorLoginManager::new);

pub struct CursorLoginManager {
    /// The in-flight attempt's cancel handle. There is only one host login, so
    /// there is at most one attempt.
    inflight: Arc<Mutex<Option<Arc<Notify>>>>,
}

impl CursorLoginManager {
    fn new() -> Self {
        CursorLoginManager {
            inflight: Arc::new(Mutex::new(None)),
        }
    }

    /// Begin a login, cancelling any earlier attempt, and return the link to
    /// send the user to. A clean exit of the spawned CLI means it stored the
    /// credentials, which [`signed_in`] then reports.
    pub async fn start(&self, cli_path: &str) -> anyhow::Result<String> {
        if let Some(prev) = self.inflight.lock().await.take() {
            prev.notify_one();
        }

        let mut cmd = crate::sandbox::SandboxedTokioCommand::new(
            cli_path,
            &crate::sandbox::SpawnScope::none(),
        );
        cmd.arg("login")
            .env("NO_OPEN_BROWSER", "1")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn `{cli_path} login`: {e}"))?;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        if let Some(stdout) = child.stdout.take() {
            spawn_line_forwarder(stdout, tx.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_line_forwarder(stderr, tx.clone());
        }
        drop(tx);

        let mut last_line = String::new();
        let url = tokio::time::timeout(URL_TIMEOUT, async {
            while let Some(line) = rx.recv().await {
                if let Some(url) = extract_login_url(&line) {
                    return Some(url);
                }
                let clean = strip_ansi(&line);
                if !clean.trim().is_empty() {
                    last_line = clean.trim().to_string();
                }
            }
            None
        })
        .await;

        let Ok(Some(url)) = url else {
            let _ = child.start_kill();
            let detail = if last_line.is_empty() {
                "it did not print a sign-in link".to_string()
            } else {
                last_line
            };
            anyhow::bail!("`cursor-agent login` failed: {detail}");
        };

        let cancel = Arc::new(Notify::new());
        let cancel_for_task = cancel.clone();
        *self.inflight.lock().await = Some(cancel);
        tokio::spawn(async move {
            // The forwarders keep draining the pipes into `rx`, which is held
            // here so a chatty CLI can't block on a closed channel.
            let _rx = rx;
            tokio::select! {
                _ = child.wait() => {}
                _ = cancel_for_task.notified() => { let _ = child.start_kill(); }
                _ = tokio::time::sleep(LOGIN_TIMEOUT) => { let _ = child.start_kill(); }
            }
            let _ = child.wait().await;
        });

        Ok(url)
    }
}

/// Pull the sign-in link out of a line of CLI output.
fn extract_login_url(line: &str) -> Option<String> {
    let clean = strip_ansi(line);
    let idx = clean.find("https://cursor.com/login")?;
    let rest = &clean[idx..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// Whether the CLI reports a signed-in host login.
pub async fn signed_in(cli_path: &str) -> bool {
    let mut cmd =
        crate::sandbox::SandboxedTokioCommand::new(cli_path, &crate::sandbox::SpawnScope::none());
    cmd.args(["status", "--format", "json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let Ok(child) = cmd.spawn() else {
        return false;
    };
    match tokio::time::timeout(STATUS_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) => parse_status(&output.stdout),
        _ => false,
    }
}

fn parse_status(stdout: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(stdout)
        .ok()
        .and_then(|v| v.get("isAuthenticated")?.as_bool())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_the_link_from_the_cli_prompt() {
        let line = "Open a browser and navigate to this link: https://cursor.com/loginDeepControl?challenge=abc&uuid=1-2&mode=login&redirectTarget=cli";
        assert_eq!(
            extract_login_url(line).as_deref(),
            Some(
                "https://cursor.com/loginDeepControl?challenge=abc&uuid=1-2&mode=login&redirectTarget=cli"
            )
        );
    }

    #[test]
    fn ignores_lines_without_a_login_link() {
        assert_eq!(
            extract_login_url("Waiting for browser authentication..."),
            None
        );
        assert_eq!(extract_login_url("Docs: https://cursor.com/docs"), None);
    }

    #[test]
    fn extracts_the_link_through_colour_codes() {
        let line = "\u{1b}[36mhttps://cursor.com/loginDeepControl?x=1\u{1b}[0m";
        assert_eq!(
            extract_login_url(line).as_deref(),
            Some("https://cursor.com/loginDeepControl?x=1")
        );
    }

    #[test]
    fn status_reads_is_authenticated() {
        assert!(parse_status(
            br#"{"status":"authenticated","isAuthenticated":true}"#
        ));
        assert!(!parse_status(
            br#"{"status":"unauthenticated","isAuthenticated":false,"message":"Not logged in"}"#
        ));
        assert!(!parse_status(b"not json"));
    }
}
