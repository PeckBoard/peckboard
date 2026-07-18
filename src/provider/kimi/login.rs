//! Browser device-code "log in with Kimi" for an account — the in-app
//! equivalent of running `kimi login` in a terminal.
//!
//! Like the Grok flow (and unlike Claude's paste-back PKCE exchange), Kimi's
//! device login is driven by the `kimi` CLI itself: it prints an
//! `Opening browser for Kimi device login:
//! https://www.kimi.com/code/authorize_device?user_code=…` line to stderr,
//! then **blocks polling** ("Waiting for authorization to complete...") until
//! the user authorises in the browser, at which point it writes its OAuth
//! tokens into `config.toml` under its `KIMI_CODE_HOME` and exits 0. So
//! [`KimiLoginManager::start`] spawns that process with the account's
//! `KIMI_CODE_HOME`, scrapes the URL, and leaves the process running in the
//! background until it completes (a non-empty `config.toml` appears → the
//! account reads as authenticated), is cancelled, or the device code expires
//! (the CLI prints "Code expires in 1800s").

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, Notify};

/// How long we wait for kimi to print the device URL before giving up.
const URL_TIMEOUT: Duration = Duration::from_secs(30);
/// Overall lifetime of a login attempt. Kimi device codes expire after
/// 1800s; a margin on top lets the CLI report its own expiry first.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(1860);

/// Process-wide registry of in-flight Kimi logins, keyed by account id. The
/// state is ephemeral (a spawned `kimi login` per account), so it lives in a
/// singleton rather than the DB.
pub static KIMI_LOGIN: LazyLock<KimiLoginManager> = LazyLock::new(KimiLoginManager::new);

struct LoginEntry {
    cancel: Arc<Notify>,
}

pub struct KimiLoginManager {
    inner: Arc<Mutex<HashMap<String, LoginEntry>>>,
}

impl KimiLoginManager {
    fn new() -> Self {
        KimiLoginManager {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Begin a device login for `account_id`, spawning `<cli_path> login`
    /// with `KIMI_CODE_HOME=config_dir`. Returns the device-login URL to
    /// send the user to once kimi prints it. Any prior in-flight login for
    /// the same account is cancelled first. The spawned process keeps
    /// running (polling Moonshot) until it exits — a clean exit writes the
    /// OAuth tokens into `config_dir/config.toml`, which is how the account
    /// later reads as authenticated.
    pub async fn start(
        &self,
        account_id: &str,
        config_dir: &str,
        cli_path: &str,
    ) -> anyhow::Result<String> {
        // Cancel any prior attempt for this account so we never leak a
        // polling process or hand back a stale URL.
        if let Some(prev) = self.inner.lock().await.remove(account_id) {
            prev.cancel.notify_one();
        }

        std::fs::create_dir_all(config_dir).ok();

        let mut cmd = Command::new(cli_path);
        cmd.arg("login")
            .env("KIMI_CODE_HOME", config_dir)
            .stdin(Stdio::null())
            // kimi prints the device prompt (and "Waiting for authorization
            // to complete...") to stderr; stdout is unused for this flow.
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn `{cli_path} login`: {e}"))?;

        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stderr handle on `kimi login`"))?;
        let mut lines = BufReader::new(stderr).lines();

        // Read stderr until kimi prints the device URL (or we give up).
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
                anyhow::bail!("timed out waiting for `kimi login` to produce a sign-in URL");
            }
        };

        // Keep the process alive in the background: it polls Moonshot and
        // writes its tokens into config.toml on success. We just drain its
        // stderr so its pipe never fills, and reap it on exit / cancel /
        // device-code expiry.
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

/// Pull the `https://www.kimi.com/code/authorize_device?user_code=…` URL out
/// of a line of kimi's stderr, if present. The URL runs to the next
/// whitespace.
pub fn extract_device_url(line: &str) -> Option<String> {
    let idx = line.find("https://www.kimi.com/code/authorize_device")?;
    let rest = &line[idx..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// Whether a `device` account has completed its login: `kimi login` writes
/// its OAuth tokens (provider + model config included) into a non-empty
/// `config.toml` under its `KIMI_CODE_HOME` on success.
pub fn device_authenticated(config_dir: Option<&str>) -> bool {
    let Some(dir) = config_dir else {
        return false;
    };
    std::path::Path::new(dir)
        .join("config.toml")
        .metadata()
        .map(|m| m.len() > 0)
        .unwrap_or(false)
}

/// Write the `config.toml` an `api_key` account runs with into its
/// `KIMI_CODE_HOME`: both Moonshot platforms as providers (the key decides
/// which of them actually accepts it) plus the standard model aliases.
/// Overwrites whatever is there — the account's key is the source of truth.
pub fn write_api_key_config(config_dir: &str, api_key: &str) -> anyhow::Result<()> {
    if api_key.chars().any(char::is_control) {
        anyhow::bail!("API key must not contain control characters");
    }
    std::fs::create_dir_all(config_dir)?;
    let key = toml_escape(api_key);
    let config = format!(
        r#"# Written by PeckBoard for this Kimi account. Overwritten whenever the
# account's API key changes — edit the account in PeckBoard, not this file.
default_model = "kimi-for-coding"

[providers.kimi-coding]
type = "kimi"
base_url = "https://api.kimi.com/coding/v1"
api_key = "{key}"

[providers.moonshot]
type = "kimi"
base_url = "https://api.moonshot.ai/v1"
api_key = "{key}"

[models.kimi-for-coding]
provider = "kimi-coding"
model = "kimi-for-coding"
max_context_size = 262144

[models.kimi-k2-thinking]
provider = "moonshot"
model = "kimi-k2-thinking"
max_context_size = 262144

[models.kimi-k2-turbo]
provider = "moonshot"
model = "kimi-k2-turbo-preview"
max_context_size = 262144
"#
    );
    std::fs::write(std::path::Path::new(config_dir).join("config.toml"), config)?;
    Ok(())
}

/// Escape a value for a TOML basic (double-quoted) string. Control
/// characters are rejected upstream, so backslash and quote are the only
/// escapes needed.
fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_device_url_from_kimi_stderr_line() {
        // The exact shape kimi prints (prefixed prose, code query param).
        let line = "Opening browser for Kimi device login: \
                    https://www.kimi.com/code/authorize_device?user_code=3RSP-27YT";
        assert_eq!(
            extract_device_url(line).as_deref(),
            Some("https://www.kimi.com/code/authorize_device?user_code=3RSP-27YT")
        );
    }

    #[test]
    fn extract_device_url_ignores_unrelated_lines() {
        assert_eq!(
            extract_device_url("Waiting for authorization to complete..."),
            None
        );
        assert_eq!(
            extract_device_url("If the browser did not open, paste the URL above"),
            None
        );
        assert_eq!(extract_device_url(""), None);
    }

    #[test]
    fn extract_device_url_stops_at_whitespace() {
        let line = "  https://www.kimi.com/code/authorize_device?user_code=AB-12  trailing";
        assert_eq!(
            extract_device_url(line).as_deref(),
            Some("https://www.kimi.com/code/authorize_device?user_code=AB-12")
        );
    }

    #[test]
    fn device_authenticated_false_without_dir_or_file() {
        assert!(!device_authenticated(None));
        assert!(!device_authenticated(Some("/nonexistent/path/xyz")));
    }

    #[test]
    fn device_authenticated_true_for_nonempty_config_toml() {
        let dir = std::env::temp_dir().join(format!("kimi-login-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), b"default_model = \"x\"").unwrap();
        assert!(device_authenticated(Some(dir.to_str().unwrap())));
        // Empty file does not count as authenticated.
        std::fs::write(dir.join("config.toml"), b"").unwrap();
        assert!(!device_authenticated(Some(dir.to_str().unwrap())));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_api_key_config_escapes_and_rejects_control_chars() {
        let dir = std::env::temp_dir().join(format!("kimi-cfg-test-{}", std::process::id()));
        write_api_key_config(dir.to_str().unwrap(), "sk-a\"b\\c").unwrap();
        let written = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(written.contains(r#"api_key = "sk-a\"b\\c""#));
        assert!(written.contains("[models.kimi-for-coding]"));
        assert!(write_api_key_config(dir.to_str().unwrap(), "sk-a\nb").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
