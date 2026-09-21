//! Local daemon configuration: server URL, enrollment token, and the
//! per-capability allow/deny flags plus the global kill-switch.
//!
//! Persisted as JSON in the per-OS config dir (via [`directories`]). The
//! security default is deny: everything is disabled **except `echo`**
//! until the machine owner explicitly opts a capability in.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

/// Every capability the daemon knows about. `echo` is the only one on by
/// default; the rest are stubs (Phase 2 real executors land later) and
/// ship disabled. Kept in one place so config defaults, the executor
/// registry, and the `hello` capability list can't drift apart.
pub const CAPABILITIES: &[&str] = &[
    "echo",
    "terminal",
    "server",
    "screenshot",
    "mouse",
    "keyboard",
];

/// The only capability enabled by default. Everything else is opt-in.
pub const DEFAULT_ENABLED: &[&str] = &["echo"];

/// On-disk daemon configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Base server URL from `enroll --server` (e.g. `https://host:3345`).
    /// [`Config::ws_url`] derives the `wss://…/ws/agent` endpoint from it.
    pub server_url: String,
    /// Enrollment secret. Sent as `Authorization: Bearer <token>` on the
    /// upgrade; the server stores only its hash and matches on that.
    pub token: String,
    /// Global kill-switch. When true, *every* capability is refused
    /// regardless of its per-capability flag — the one lever that stops
    /// all remote control instantly.
    pub kill_switch: bool,
    /// Per-capability enabled flags. Absent ⇒ disabled.
    pub capabilities: BTreeMap<String, bool>,
    /// Owner-configured managed processes the `server` capability may
    /// control, keyed by a friendly name. The server can only start a
    /// process the machine owner named here — never an arbitrary command
    /// line — so remote control can't escalate `server` into `terminal`.
    /// `#[serde(default)]` keeps configs written before this field loading.
    #[serde(default)]
    pub servers: BTreeMap<String, ManagedServer>,
}
/// One owner-configured managed process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedServer {
    /// Command line, run through the platform shell when started.
    pub command: String,
    /// Optional working directory for the process.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Optional health-check command; exit 0 ⇒ healthy. Absent ⇒ health
    /// falls back to "is the managed process still running".
    #[serde(default)]
    pub health_command: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        let capabilities = CAPABILITIES
            .iter()
            .map(|c| (c.to_string(), DEFAULT_ENABLED.contains(c)))
            .collect();
        Self {
            server_url: String::new(),
            token: String::new(),
            kill_switch: false,
            capabilities,
            servers: BTreeMap::new(),
        }
    }
}

impl Config {
    /// Whether a capability may run right now: never while the kill-switch
    /// is on, otherwise only if its per-capability flag is explicitly true.
    pub fn is_enabled(&self, capability: &str) -> bool {
        !self.kill_switch && self.capabilities.get(capability).copied().unwrap_or(false)
    }

    /// The capabilities to advertise in the `hello` frame — exactly those
    /// the daemon will actually serve given the current flags/kill-switch.
    pub fn enabled_capabilities(&self) -> Vec<String> {
        CAPABILITIES
            .iter()
            .filter(|c| self.is_enabled(c))
            .map(|c| c.to_string())
            .collect()
    }

    /// Derive the WebSocket endpoint from `server_url`: map the HTTP scheme
    /// to the WS scheme and append `/ws/agent` if not already present.
    pub fn ws_url(&self) -> String {
        let base = self.server_url.trim_end_matches('/');
        let base = if let Some(rest) = base.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = base.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            base.to_string()
        };
        if base.ends_with("/ws/agent") {
            base
        } else {
            format!("{base}/ws/agent")
        }
    }

    /// Default config-file path: `<per-OS config dir>/config.json`.
    pub fn config_path() -> anyhow::Result<PathBuf> {
        let dirs = ProjectDirs::from("board", "Peck", "peckboard-agent")
            .context("could not determine the per-OS config directory")?;
        Ok(dirs.config_dir().join("config.json"))
    }

    /// Default audit-log path: `<per-OS config dir>/audit.jsonl`, beside
    /// the config file.
    pub fn audit_path() -> anyhow::Result<PathBuf> {
        Ok(Self::config_path()?
            .parent()
            .expect("config path always has a parent dir")
            .join("audit.jsonl"))
    }

    /// Load from the default path.
    pub fn load() -> anyhow::Result<Self> {
        Self::load_from(&Self::config_path()?)
    }

    /// Load from an explicit path (used by tests).
    pub fn load_from(path: &Path) -> anyhow::Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading config at {}", path.display()))?;
        serde_json::from_str(&text).context("parsing config JSON")
    }

    /// Save to the default path, creating parent dirs as needed.
    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&Self::config_path()?)
    }

    /// Save to an explicit path (used by tests). The file holds the
    /// long-lived enrollment token, so on Unix it is created `0600` inside
    /// a `0700` directory — the same treatment ssh/gh/aws give credential
    /// files. Windows relies on the per-user profile ACL.
    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating config dir {}", parent.display()))?;
            restrict_to_owner(parent, 0o700);
        }
        let json = serde_json::to_string_pretty(self).context("serializing config")?;
        write_owner_only(path, &json)
            .with_context(|| format!("writing config to {}", path.display()))?;
        // Heal files created before the 0600 policy (the create mode only
        // applies to new files).
        restrict_to_owner(path, 0o600);
        Ok(())
    }
}

/// Best-effort `chmod` — owner-only credential hygiene on Unix; no-op
/// elsewhere.
#[cfg(unix)]
fn restrict_to_owner(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}
#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path, _mode: u32) {}

/// Create/truncate `path` with owner-only permissions from the first byte
/// (a plain `fs::write` would leave a umask-default window).
#[cfg(unix)]
fn write_owner_only(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(contents.as_bytes())
}
#[cfg(not(unix))]
fn write_owner_only(path: &Path, contents: &str) -> std::io::Result<()> {
    fs::write(path, contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_denies_all_but_echo() {
        let cfg = Config::default();
        assert!(cfg.is_enabled("echo"));
        assert!(!cfg.is_enabled("terminal"));
        assert!(!cfg.is_enabled("mouse"));
        assert_eq!(cfg.enabled_capabilities(), vec!["echo".to_string()]);
    }

    #[test]
    fn kill_switch_disables_everything() {
        let mut cfg = Config::default();
        cfg.capabilities.insert("terminal".into(), true);
        assert!(cfg.is_enabled("terminal"));
        cfg.kill_switch = true;
        assert!(!cfg.is_enabled("terminal"));
        assert!(!cfg.is_enabled("echo"));
        assert!(cfg.enabled_capabilities().is_empty());
    }

    #[test]
    fn config_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.json");

        let mut cfg = Config {
            server_url: "https://box.example:3345".into(),
            token: "s3cr3t".into(),
            kill_switch: true,
            ..Default::default()
        };
        cfg.capabilities.insert("terminal".into(), true);

        cfg.save_to(&path).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(cfg, loaded);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "config file must be owner-only");
        }
    }

    #[test]
    fn config_without_servers_field_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        // A config written before `servers` existed.
        let json = r#"{"server_url":"https://box:3345","token":"t","kill_switch":false,"capabilities":{"echo":true}}"#;
        std::fs::write(&path, json).unwrap();
        let cfg = Config::load_from(&path).unwrap();
        assert!(cfg.servers.is_empty());
        assert!(cfg.is_enabled("echo"));
    }

    #[test]
    fn ws_url_maps_scheme_and_appends_path() {
        let mut cfg = Config {
            server_url: "https://box:3345".into(),
            ..Default::default()
        };
        cfg.server_url = "https://box:3345".into();
        assert_eq!(cfg.ws_url(), "wss://box:3345/ws/agent");
        cfg.server_url = "http://localhost:3344/".into();
        assert_eq!(cfg.ws_url(), "ws://localhost:3344/ws/agent");
        cfg.server_url = "wss://box/ws/agent".into();
        assert_eq!(cfg.ws_url(), "wss://box/ws/agent");
    }
}
