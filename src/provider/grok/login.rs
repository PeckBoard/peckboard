//! Browser device-code "log in with Grok" for an account — the in-app
//! equivalent of running `grok login --device-auth` in a terminal.
//!
//! Unlike the Claude flow (a self-contained PKCE exchange where the user
//! pastes a `code#state` back), Grok's device login is driven by the `grok`
//! CLI itself: it prints a `https://accounts.x.ai/oauth2/device?user_code=…`
//! URL to stderr, then **blocks polling** until the user authorises in the
//! browser, at which point it writes credentials into its `GROK_HOME`
//! (`auth.json`). So [`GrokLoginManager::start`] spawns that process with the
//! account's `GROK_HOME`, scrapes the URL, and leaves the process running in
//! the background until it completes (auth.json appears → the account reads as
//! authenticated), is cancelled, or the device code expires.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, Notify};

/// How long we wait for grok to print the device URL before giving up.
const URL_TIMEOUT: Duration = Duration::from_secs(30);
/// Overall lifetime of a login attempt (xAI device codes expire; after this
/// we stop waiting and reap the process).
const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);

/// Process-wide registry of in-flight Grok logins, keyed by account id. The
/// state is ephemeral (a spawned `grok login` per account), so it lives in a
/// singleton rather than the DB.
pub static GROK_LOGIN: LazyLock<GrokLoginManager> = LazyLock::new(GrokLoginManager::new);

struct LoginEntry {
    cancel: Arc<Notify>,
}

pub struct GrokLoginManager {
    inner: Arc<Mutex<HashMap<String, LoginEntry>>>,
}

impl GrokLoginManager {
    fn new() -> Self {
        GrokLoginManager {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Begin a device login for `account_id`, spawning `grok login
    /// --device-auth` with `GROK_HOME=config_dir`. Returns the device-login
    /// URL to send the user to once grok prints it. Any prior in-flight login
    /// for the same account is cancelled first. The spawned process keeps
    /// running (polling xAI) until it exits — a clean exit writes
    /// `config_dir/auth.json`, which is how the account later reads as
    /// authenticated.
    pub async fn start(&self, account_id: &str, config_dir: &str) -> anyhow::Result<String> {
        // Cancel any prior attempt for this account so we never leak a
        // polling process or hand back a stale URL.
        if let Some(prev) = self.inner.lock().await.remove(account_id) {
            prev.cancel.notify_one();
        }

        std::fs::create_dir_all(config_dir).ok();

        let mut cmd = Command::new("grok");
        cmd.args(["login", "--device-auth"])
            .env("GROK_HOME", config_dir)
            .stdin(Stdio::null())
            // grok prints the device prompt (and "Waiting for
            // authorization…") to stderr; stdout is unused for this flow.
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn `grok login`: {e}"))?;

        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stderr handle on `grok login`"))?;
        let mut lines = BufReader::new(stderr).lines();

        // Read stderr until grok prints the device URL (or we give up).
        let url = tokio::time::timeout(URL_TIMEOUT, async {
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(url) = extract_device_url(&line) {
                    return Some(url);
                }
            }
            None
        })
        .await;

        let url = match url {
            Ok(Some(url)) => url,
            _ => {
                let _ = child.start_kill();
                anyhow::bail!("timed out waiting for `grok login` to produce a sign-in URL");
            }
        };

        // Keep the process alive in the background: it polls xAI and writes
        // auth.json on success. We just drain its stderr so its pipe never
        // fills, and reap it on exit / cancel / device-code expiry.
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
                    line = lines.next_line() => {
                        match line {
                            Ok(Some(_)) => continue, // drain
                            _ => break,              // EOF / error → process is exiting
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

        Ok(url)
    }

    /// Cancel any in-flight login for `account_id` (e.g. on account delete).
    pub async fn cancel(&self, account_id: &str) {
        if let Some(entry) = self.inner.lock().await.remove(account_id) {
            entry.cancel.notify_one();
        }
    }
}

/// Pull the `https://accounts.x.ai/oauth2/device?user_code=…` URL out of a
/// line of grok's stderr, if present. The URL runs to the next whitespace.
pub fn extract_device_url(line: &str) -> Option<String> {
    let idx = line.find("https://accounts.x.ai/oauth2/device")?;
    let rest = &line[idx..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// Whether a `device` account has completed its login: grok writes a
/// non-empty `auth.json` into its `GROK_HOME` on success.
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
    fn extract_device_url_from_grok_stderr_line() {
        // The exact shape grok prints (indented, trailing whitespace).
        let line = "  https://accounts.x.ai/oauth2/device?user_code=9ATF-W2ZC  ";
        assert_eq!(
            extract_device_url(line).as_deref(),
            Some("https://accounts.x.ai/oauth2/device?user_code=9ATF-W2ZC")
        );
    }

    #[test]
    fn extract_device_url_ignores_unrelated_lines() {
        assert_eq!(extract_device_url("Waiting for authorization..."), None);
        assert_eq!(extract_device_url("Confirm this code: 9ATF-W2ZC"), None);
        assert_eq!(extract_device_url(""), None);
    }

    #[test]
    fn extract_device_url_handles_no_trailing_whitespace() {
        let line = "https://accounts.x.ai/oauth2/device?user_code=ABCD-1234";
        assert_eq!(extract_device_url(line).as_deref(), Some(line));
    }

    #[test]
    fn device_authenticated_false_without_dir_or_file() {
        assert!(!device_authenticated(None));
        assert!(!device_authenticated(Some("/nonexistent/path/xyz")));
    }

    #[test]
    fn device_authenticated_true_for_nonempty_auth_json() {
        let dir = std::env::temp_dir().join(format!("grok-login-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("auth.json"), b"{\"token\":\"x\"}").unwrap();
        assert!(device_authenticated(Some(dir.to_str().unwrap())));
        // Empty file does not count as authenticated.
        std::fs::write(dir.join("auth.json"), b"").unwrap();
        assert!(!device_authenticated(Some(dir.to_str().unwrap())));
        std::fs::remove_dir_all(&dir).ok();
    }
}
