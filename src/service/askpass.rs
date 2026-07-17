//! Sudo askpass bridge: lets an agent session run `sudo -A <cmd>` and have
//! the password typed by the user in the web UI instead of on a TTY the
//! headless CLI child doesn't have.
//!
//! Flow: dispatch injects `SUDO_ASKPASS=<data_dir>/askpass.sh` plus a
//! per-session secret token into the CLI child's environment (see
//! `SessionManager::send_message_locked`). When sudo needs a password it
//! execs the helper, which POSTs the prompt + token to `POST /api/askpass`
//! and blocks. That route broadcasts an `askpass-request` WS event, the UI
//! shows a masked password dialog, and `POST
//! /api/sessions/{id}/askpass-answer` resolves the pending request — the
//! helper prints the password on stdout and sudo consumes it.
//!
//! The password only ever lives in the oneshot channel below and the
//! helper→sudo pipe: it is never persisted, never logged, and never enters
//! the agent transcript.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{Mutex, oneshot};

/// How long `POST /api/askpass` holds the helper's request open waiting for
/// the user. The helper's own `curl --max-time` (in the script below) is
/// slightly above this so the server, not curl, decides the timeout.
pub const ANSWER_TIMEOUT_SECS: u64 = 170;

/// Everything dispatch needs to wire `sudo -A` into a spawned CLI child.
#[derive(Clone)]
pub struct AskpassEnv {
    pub registry: AskpassRegistry,
    /// Absolute path of the helper script (`SUDO_ASKPASS` value).
    pub script_path: String,
    /// Loopback URL of `POST /api/askpass` (`PECKBOARD_ASKPASS_URL` value).
    pub url: String,
}

#[derive(Default)]
struct Inner {
    /// sha256(token) → session_id. Tokens are stored hashed, like
    /// [`crate::service::mcp_server::McpTokenRegistry`].
    tokens: HashMap<String, String>,
    /// session_id → sha256(token), so re-issuing on a fresh spawn replaces
    /// (invalidates) the session's previous token instead of accumulating.
    by_session: HashMap<String, String>,
    /// request_id → answer channel for in-flight password requests.
    /// `Some(password)` = user submitted; `None` = user cancelled.
    pending: HashMap<String, oneshot::Sender<Option<String>>>,
}

/// In-memory registry of per-session askpass tokens and pending password
/// requests. Cheap to clone (shared `Arc` inner).
#[derive(Clone, Default)]
pub struct AskpassRegistry {
    inner: Arc<Mutex<Inner>>,
}

fn sha256_hex(s: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(s.as_bytes()))
}

impl AskpassRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Issue (or rotate) the askpass token for `session_id`; returns the raw
    /// token to place in the child's `PECKBOARD_ASKPASS_TOKEN`.
    pub async fn issue_token(&self, session_id: &str) -> String {
        use rand::Rng;
        let mut raw = [0u8; 24];
        rand::thread_rng().fill(&mut raw);
        let token = hex::encode(raw);
        let hash = sha256_hex(&token);

        let mut g = self.inner.lock().await;
        if let Some(old) = g.by_session.remove(session_id) {
            g.tokens.remove(&old);
        }
        g.tokens.insert(hash.clone(), session_id.to_string());
        g.by_session.insert(session_id.to_string(), hash);
        token
    }

    /// Resolve a raw token to its session id, or `None` for unknown tokens.
    pub async fn session_for_token(&self, token: &str) -> Option<String> {
        let hash = sha256_hex(token);
        self.inner.lock().await.tokens.get(&hash).cloned()
    }

    /// Register a pending password request; the receiver resolves when the
    /// user answers (or is dropped when the request times out).
    pub async fn begin_request(&self) -> (String, oneshot::Receiver<Option<String>>) {
        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.inner
            .lock()
            .await
            .pending
            .insert(request_id.clone(), tx);
        (request_id, rx)
    }

    /// Deliver the user's answer (`Some(password)` or `None` for cancel).
    /// Returns false when the request is unknown — already answered, timed
    /// out, or never existed.
    pub async fn resolve(&self, request_id: &str, answer: Option<String>) -> bool {
        let tx = self.inner.lock().await.pending.remove(request_id);
        match tx {
            Some(tx) => tx.send(answer).is_ok(),
            None => false,
        }
    }

    /// Drop a pending request without answering (timeout path). The waiting
    /// helper request has already given up; a late answer gets `false` from
    /// [`Self::resolve`].
    pub async fn drop_request(&self, request_id: &str) {
        self.inner.lock().await.pending.remove(request_id);
    }
}

/// Write the sudo askpass helper into `data_dir` and return its absolute
/// path. Idempotent — rewritten on every startup so upgrades ship script
/// fixes. Owner-only exec (0700): the token in the environment is the real
/// gate, but there is no reason for other local users to read the script.
pub fn write_askpass_script(data_dir: &Path) -> std::io::Result<PathBuf> {
    let path = data_dir.join("askpass.sh");
    // sudo invokes the helper as `askpass.sh "<prompt>"` and reads the
    // password from its stdout. curl's --max-time sits above the server's
    // ANSWER_TIMEOUT_SECS so the server-side timeout (which also dismisses
    // the UI dialog) fires first.
    let script = "#!/bin/sh\n\
# Peckboard sudo askpass helper (generated at startup — do not edit).\n\
# Asks the Peckboard UI for a password: run commands as `sudo -A <cmd>`\n\
# inside a Peckboard session and a masked dialog appears for the user.\n\
exec curl -fsS --max-time 175 \\\n\
  -H \"X-Peckboard-Askpass-Token: ${PECKBOARD_ASKPASS_TOKEN}\" \\\n\
  --data-urlencode \"prompt=${1:-Password:}\" \\\n\
  \"${PECKBOARD_ASKPASS_URL}\"\n";
    std::fs::write(&path, script)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn issue_and_lookup_roundtrip() {
        let reg = AskpassRegistry::new();
        let token = reg.issue_token("sess-1").await;
        assert_eq!(token.len(), 48); // 24 bytes → 48 hex chars
        assert_eq!(
            reg.session_for_token(&token).await.as_deref(),
            Some("sess-1")
        );
        assert!(reg.session_for_token("bogus").await.is_none());
    }

    #[tokio::test]
    async fn reissue_rotates_the_old_token_out() {
        let reg = AskpassRegistry::new();
        let t1 = reg.issue_token("sess-1").await;
        let t2 = reg.issue_token("sess-1").await;
        assert!(reg.session_for_token(&t1).await.is_none());
        assert_eq!(reg.session_for_token(&t2).await.as_deref(), Some("sess-1"));
    }

    #[tokio::test]
    async fn resolve_delivers_password_once() {
        let reg = AskpassRegistry::new();
        let (id, rx) = reg.begin_request().await;
        assert!(reg.resolve(&id, Some("hunter2".into())).await);
        assert_eq!(rx.await.unwrap().as_deref(), Some("hunter2"));
        // Second resolve for the same id: request is gone.
        assert!(!reg.resolve(&id, Some("again".into())).await);
    }

    #[tokio::test]
    async fn cancel_and_timeout_paths() {
        let reg = AskpassRegistry::new();
        let (id, rx) = reg.begin_request().await;
        assert!(reg.resolve(&id, None).await);
        assert_eq!(rx.await.unwrap(), None);

        let (id2, rx2) = reg.begin_request().await;
        reg.drop_request(&id2).await;
        assert!(rx2.await.is_err()); // sender dropped → RecvError
        assert!(!reg.resolve(&id2, Some("late".into())).await);
    }

    #[test]
    fn script_is_written_executable() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_askpass_script(dir.path()).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("#!/bin/sh"));
        assert!(content.contains("PECKBOARD_ASKPASS_TOKEN"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }
}
