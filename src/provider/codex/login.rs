//! Browser device-code "Sign in with ChatGPT" for a Codex account — the
//! in-app equivalent of running `codex login --device-auth` in a terminal.
//!
//! Like the Grok/Kimi flows (and unlike Claude's paste-back PKCE exchange),
//! Codex's ChatGPT login is driven by the `codex` CLI itself: it prints a
//! verification URL (`https://auth.openai.com/codex/device`) and a one-time
//! user code to stdout, then **blocks polling** until the user authorises
//! in the browser, at which point it writes tokens into `auth.json` under
//! its `CODEX_HOME` and exits 0. So [`CodexLoginManager::start`] spawns
//! that process with the account's `CODEX_HOME`, scrapes the URL + code,
//! and leaves the process running in the background until it completes
//! (`auth.json` appears → the account reads as authenticated), is cancelled,
//! or the device code expires (15 minutes).

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, Notify};

/// How long we wait for codex to print the device URL + code before giving up.
const URL_TIMEOUT: Duration = Duration::from_secs(30);
/// Overall lifetime of a login attempt. Codex device codes expire after
/// 15 minutes; a margin on top lets the CLI report its own expiry first.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(960);

/// Process-wide registry of in-flight Codex logins, keyed by account id. The
/// state is ephemeral (a spawned `codex login --device-auth` per account), so
/// it lives in a singleton rather than the DB.
pub static CODEX_LOGIN: LazyLock<CodexLoginManager> = LazyLock::new(CodexLoginManager::new);

struct LoginEntry {
    cancel: Arc<Notify>,
}

pub struct CodexLoginManager {
    inner: Arc<Mutex<HashMap<String, LoginEntry>>>,
}

/// URL + one-time code scraped from `codex login --device-auth` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceLogin {
    pub url: String,
    pub user_code: String,
}

impl CodexLoginManager {
    fn new() -> Self {
        CodexLoginManager {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Begin a ChatGPT device login for `account_id`, spawning
    /// `<cli_path> login --device-auth` with `CODEX_HOME=config_dir`.
    /// Returns the verification URL and one-time code once codex prints
    /// them. Any prior in-flight login for the same account is cancelled
    /// first. The spawned process keeps running (polling OpenAI) until it
    /// exits — a clean exit writes `config_dir/auth.json`, which is how
    /// the account later reads as authenticated.
    pub async fn start(
        &self,
        account_id: &str,
        config_dir: &str,
        cli_path: &str,
    ) -> anyhow::Result<DeviceLogin> {
        // Cancel any prior attempt for this account so we never leak a
        // polling process or hand back a stale URL.
        if let Some(prev) = self.inner.lock().await.remove(account_id) {
            prev.cancel.notify_one();
        }

        std::fs::create_dir_all(config_dir).ok();

        let mut cmd = Command::new(cli_path);
        cmd.args(["login", "--device-auth"])
            .env("CODEX_HOME", config_dir)
            // ChatGPT sign-in, not API-key: drop env that would flip the CLI
            // onto the API-key path.
            .env_remove("CODEX_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .stdin(Stdio::null())
            // The device prompt is `println!` (stdout). Some builds also
            // chatter on stderr; capture both so we don't miss the URL.
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!("failed to spawn `{cli_path} login --device-auth`: {e}")
        })?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stdout handle on `codex login`"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stderr handle on `codex login`"))?;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        spawn_line_forwarder(stdout, tx.clone());
        spawn_line_forwarder(stderr, tx);

        // Read until codex prints the device URL + one-time code (or we give up).
        let prompt = tokio::time::timeout(URL_TIMEOUT, async {
            let mut acc = String::new();
            while let Some(line) = rx.recv().await {
                acc.push_str(&line);
                acc.push('\n');
                if let Some(p) = extract_device_login(&acc) {
                    return Some(p);
                }
            }
            None
        })
        .await;

        let prompt = match prompt {
            Ok(Some(p)) => p,
            _ => {
                let _ = child.start_kill();
                anyhow::bail!(
                    "timed out waiting for `codex login --device-auth` to produce a ChatGPT sign-in URL"
                );
            }
        };

        // Keep the process alive in the background: it polls OpenAI and
        // writes auth.json on success. Drain leftover output so the pipe
        // never fills, and reap it on exit / cancel / device-code expiry.
        let cancel = Arc::new(Notify::new());
        let cancel_for_task = cancel.clone();
        let map = self.inner.clone();
        let id = account_id.to_string();
        tokio::spawn(async move {
            let deadline = tokio::time::sleep(LOGIN_TIMEOUT);
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    _ = cancel_for_task.notified() => {
                        let _ = child.start_kill();
                        break;
                    }
                    _ = &mut deadline => {
                        let _ = child.start_kill();
                        break;
                    }
                    line = rx.recv() => {
                        match line {
                            Some(_) => continue, // drain
                            None => break,       // both pipes EOF → process is exiting
                        }
                    }
                }
            }
            let _ = child.wait().await;
            map.lock().await.remove(&id);
        });

        self.inner
            .lock()
            .await
            .insert(account_id.to_string(), LoginEntry { cancel });

        Ok(prompt)
    }

    /// Cancel any in-flight login for `account_id` (e.g. on account delete).
    pub async fn cancel(&self, account_id: &str) {
        if let Some(entry) = self.inner.lock().await.remove(account_id) {
            entry.cancel.notify_one();
        }
    }
}

fn spawn_line_forwarder<R>(reader: R, tx: tokio::sync::mpsc::UnboundedSender<String>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
}

/// Strip CSI SGR sequences (`ESC[…m`) so URL/code matching isn't thrown by
/// the CLI's colour codes.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for n in chars.by_ref() {
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Pull the ChatGPT device-login URL (`…/codex/device`) and one-time code
/// (`XXXX-XXXX`) out of accumulated CLI output, if both are present.
pub fn extract_device_login(text: &str) -> Option<DeviceLogin> {
    let clean = strip_ansi(text);
    let url = extract_device_url(&clean)?;
    let user_code = extract_user_code(&clean)?;
    Some(DeviceLogin { url, user_code })
}

fn extract_device_url(text: &str) -> Option<String> {
    let mut rest = text;
    while let Some(idx) = rest.find("https://") {
        let candidate = &rest[idx..];
        let end = candidate
            .find(|c: char| c.is_whitespace())
            .unwrap_or(candidate.len());
        let url = candidate[..end]
            .trim_end_matches(['.', ',', ';', ')', ']'])
            .to_string();
        if url.contains("/codex/device") {
            return Some(url);
        }
        rest = &rest[idx + 8..];
    }
    None
}

fn extract_user_code(text: &str) -> Option<String> {
    for token in text.split_whitespace() {
        let t = token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-');
        if is_device_code(t) {
            return Some(t.to_string());
        }
    }
    None
}
/// Codex device codes are uppercase alphanumeric groups joined by a hyphen —
/// `4UWK-LDLPZ` on codex 0.153.4. Group lengths have shifted between releases
/// (an earlier build used 4-4), so match the shape rather than exact lengths.
/// Requiring uppercase keeps hyphenated prose (`command-line`, `one-time`) out.
fn is_device_code(s: &str) -> bool {
    let mut parts = s.split('-');
    let (Some(a), Some(b), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let group_ok = |p: &str| {
        (3..=8).contains(&p.len())
            && p.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    };
    group_ok(a) && group_ok(b)
}
/// Whether a `device` account has completed its login: `codex login
/// --device-auth` writes a non-empty `auth.json` into its `CODEX_HOME`.
pub fn device_authenticated(config_dir: Option<&str>) -> bool {
    let Some(dir) = config_dir else {
        return false;
    };
    std::path::Path::new(dir)
        .join("auth.json")
        .metadata()
        .map(|m| m.len() > 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_device_login_from_codex_prompt() {
        // Verbatim capture from codex 0.153.4 `login --device-auth` (ANSI
        // wrapping the URL and code). Note the 4-5 code shape.
        let prompt = "\nWelcome to Codex [v\x1b[90m0.153.4\x1b[0m]\n\
\x1b[90mOpenAI's command-line coding agent\x1b[0m\n\
\nFollow these steps to sign in with ChatGPT using device code authorization:\n\
\n1. Open this link in your browser and sign in to your account\n   \x1b[94mhttps://auth.openai.com/codex/device\x1b[0m\n\
\n2. Enter this one-time code \x1b[90m(expires in 15 minutes)\x1b[0m\n   \x1b[94m4UWK-LDLPZ\x1b[0m\n";
        let got = extract_device_login(prompt).expect("prompt should parse");
        assert_eq!(got.url, "https://auth.openai.com/codex/device");
        assert_eq!(got.user_code, "4UWK-LDLPZ");
    }

    #[test]
    fn extract_device_login_accepts_older_four_four_code() {
        let text = "https://auth.openai.com/codex/device\nABCD-EFGH\n";
        let got = extract_device_login(text).expect("4-4 codes still parse");
        assert_eq!(got.user_code, "ABCD-EFGH");
    }

    #[test]
    fn extract_user_code_ignores_hyphenated_prose() {
        assert!(extract_user_code("OpenAI's command-line coding agent\n").is_none());
        assert!(extract_user_code("Enter this one-time code\n").is_none());
    }

    #[test]
    fn extract_device_login_ignores_unrelated_https() {
        let text = "see https://learn.chatgpt.com/docs/codex/cli for help\n";
        assert!(extract_device_login(text).is_none());
    }

    #[test]
    fn extract_device_login_needs_both_url_and_code() {
        assert!(extract_device_login("https://auth.openai.com/codex/device\n").is_none());
        assert!(extract_device_login("ABCD-EFGH\n").is_none());
    }

    #[test]
    fn device_authenticated_false_without_dir_or_file() {
        assert!(!device_authenticated(None));
        assert!(!device_authenticated(Some("/nonexistent/xyz-codex-login")));
    }

    #[test]
    fn device_authenticated_true_for_nonempty_auth_json() {
        let dir = std::env::temp_dir().join(format!("codex-login-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("auth.json"), "{\"tokens\":{}}").unwrap();
        assert!(device_authenticated(Some(dir.to_str().unwrap())));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
