//! Move a CLI account's credential file aside for the duration of a
//! re-login, and put it back if the attempt never produces fresh
//! credentials.
//!
//! Every device-code provider here (codex, grok, kimi) decides "is this
//! account signed in?" by looking for a non-empty credential file in the
//! account's home (`auth.json`, `config.toml`). That makes a *broken*
//! account indistinguishable from a working one: the file is still there,
//! so the account reads as authenticated forever, and the CLI we spawn to
//! re-login may short-circuit on the stale credentials instead of printing
//! a fresh device prompt. Stashing the file before spawning fixes both —
//! the CLI starts from a clean home, and the account honestly reads "not
//! signed in" until a new login lands.
//!
//! [`CredentialStash::settle`] is the counterpart: it restores the stashed
//! file when the attempt produced nothing, so abandoning a re-sign-in
//! leaves the account exactly as it was.

use std::path::{Path, PathBuf};

/// Suffix for the moved-aside credential file. Sits next to the original in
/// the account home, which is already private to that account.
const BACKUP_SUFFIX: &str = ".peckboard-bak";

/// A credential file moved out of the way while a login runs. Created by
/// [`CredentialStash::stash`]; resolve it with [`CredentialStash::settle`]
/// once the attempt has finished (success, cancel, timeout, or spawn
/// failure).
#[derive(Debug)]
pub struct CredentialStash {
    live: PathBuf,
    backup: PathBuf,
    /// Whether we actually moved a file (nothing to restore if not).
    stashed: bool,
}

impl CredentialStash {
    /// Move `config_dir/file_name` aside if it exists and is non-empty. An
    /// absent, empty, or unmovable file is a no-op — the login proceeds
    /// either way; this is best-effort hygiene, not a gate.
    pub fn stash(config_dir: &str, file_name: &str) -> Self {
        let live = Path::new(config_dir).join(file_name);
        let backup = Path::new(config_dir).join(format!("{file_name}{BACKUP_SUFFIX}"));
        let has_creds = live.metadata().map(|m| m.len() > 0).unwrap_or(false);
        let stashed = has_creds && std::fs::rename(&live, &backup).is_ok();
        if has_creds && !stashed {
            tracing::warn!(path = %live.display(), "login: could not stash existing credentials");
        }
        CredentialStash {
            live,
            backup,
            stashed,
        }
    }

    /// Finish up: keep fresh credentials if the login wrote any, otherwise
    /// put the stashed ones back. Safe to call from the background task
    /// that reaps the login process.
    pub fn settle(self) {
        if !self.stashed {
            return;
        }
        let fresh = self.live.metadata().map(|m| m.len() > 0).unwrap_or(false);
        if fresh {
            let _ = std::fs::remove_file(&self.backup);
        } else if let Err(e) = std::fs::rename(&self.backup, &self.live) {
            tracing::warn!(path = %self.live.display(), "login: could not restore credentials: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique temp dir for one test. `std::env::temp_dir` + pid + name
    /// keeps this dependency-free, matching the login modules' own tests.
    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "peckboard-stash-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn stash_moves_creds_aside_so_the_account_reads_signed_out() {
        let dir = tmp_dir("moves");
        std::fs::write(dir.join("auth.json"), b"{\"stale\":true}").unwrap();

        let stash = CredentialStash::stash(dir.to_str().unwrap(), "auth.json");
        assert!(!dir.join("auth.json").exists());
        assert!(dir.join("auth.json.peckboard-bak").exists());

        // Nothing fresh landed → the old credentials come back untouched.
        stash.settle();
        assert_eq!(
            std::fs::read_to_string(dir.join("auth.json")).unwrap(),
            "{\"stale\":true}"
        );
        assert!(!dir.join("auth.json.peckboard-bak").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn settle_keeps_fresh_creds_and_drops_the_backup() {
        let dir = tmp_dir("fresh");
        std::fs::write(dir.join("auth.json"), b"old").unwrap();

        let stash = CredentialStash::stash(dir.to_str().unwrap(), "auth.json");
        // The CLI completed a login and wrote new credentials.
        std::fs::write(dir.join("auth.json"), b"new").unwrap();
        stash.settle();

        assert_eq!(
            std::fs::read_to_string(dir.join("auth.json")).unwrap(),
            "new"
        );
        assert!(!dir.join("auth.json.peckboard-bak").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stash_is_a_noop_without_existing_creds() {
        let dir = tmp_dir("empty");
        std::fs::write(dir.join("config.toml"), b"").unwrap();

        let stash = CredentialStash::stash(dir.to_str().unwrap(), "config.toml");
        assert!(!dir.join("config.toml.peckboard-bak").exists());
        stash.settle();
        // An empty file is left exactly as it was, not deleted or restored.
        assert!(dir.join("config.toml").exists());

        let missing = CredentialStash::stash(dir.to_str().unwrap(), "nothing.json");
        missing.settle();
        assert!(!dir.join("nothing.json").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
