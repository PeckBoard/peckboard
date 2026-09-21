//! Local audit log: every capability request the daemon handles is
//! appended here as one JSON object per line (JSONL), and the same entry
//! is mirrored to the server as an [`AgentFrame::Event`] (`kind: "audit"`).
//!
//! Refusals count: a request blocked by the kill-switch or a disabled
//! capability is audited with `outcome: "refused"` so the local log is a
//! complete record of what was *asked* of this machine, not just what ran.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// How a request resolved, as recorded in the audit log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Executor ran and returned success.
    Ok,
    /// Executor ran and returned an error.
    Error,
    /// Blocked before dispatch (kill-switch or capability disabled).
    Refused,
}

/// One audit record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Unix time in milliseconds when the request was handled.
    pub ts_ms: u128,
    pub corr_id: String,
    pub capability: String,
    pub outcome: Outcome,
    /// Present when `outcome` is `error` or `refused`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl AuditEntry {
    /// Build an entry stamped with the current wall-clock time.
    pub fn now(corr_id: &str, capability: &str, outcome: Outcome, error: Option<String>) -> Self {
        Self {
            ts_ms: now_ms(),
            corr_id: corr_id.to_string(),
            capability: capability.to_string(),
            outcome,
            error,
        }
    }

    /// The entry as a JSON value, for mirroring in an event frame.
    pub fn to_value(&self) -> Value {
        json!(self)
    }
}

/// Append-only audit log backed by a JSONL file.
pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Append one entry as a JSON line. Best-effort: a failure to write is
    /// logged and swallowed rather than killing the request path — the
    /// server-mirrored event still records it.
    pub fn append(&self, entry: &AuditEntry) {
        if let Err(e) = self.try_append(entry) {
            tracing::warn!(error = %e, path = %self.path.display(), "audit append failed");
        }
    }

    fn try_append(&self, entry: &AuditEntry) -> std::io::Result<()> {
        self.try_append_json(&serde_json::to_string(entry)?)
    }

    fn try_append_json(&self, json_line: &str) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut line = json_line.to_string();
        line.push('\n');
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        // The log records capability activity on this machine — keep it
        // owner-only, like the config.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        let mut f = opts.open(&self.path)?;
        f.write_all(line.as_bytes())
    }

    #[cfg(test)]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

/// Process-wide audit log, set once at startup by [`init`]. Executors that
/// want to record action-level detail call [`record`]; the per-request
/// entries (corr_id + outcome, mirrored to the server) are written by
/// `crate::client` independently.
static GLOBAL: std::sync::OnceLock<AuditLog> = std::sync::OnceLock::new();

/// Point the process-wide audit log at `path`. First call wins; later
/// calls are ignored.
pub fn init(path: PathBuf) {
    let _ = GLOBAL.set(AuditLog::new(path));
}

/// Append an executor-level action record to the process-wide audit log.
/// Best-effort and synchronous; a no-op (logged at debug) before [`init`].
pub fn record(capability: &str, action: &str, ok: bool, detail: Value) {
    let entry = json!({
        "ts_ms": now_ms(),
        "capability": capability,
        "action": action,
        "ok": ok,
        "detail": detail,
    });
    match GLOBAL.get() {
        Some(log) => {
            if let Err(e) = log.try_append_json(&entry.to_string()) {
                tracing::warn!(error = %e, "audit record append failed");
            }
        }
        None => tracing::debug!(
            capability,
            action,
            "audit::record before init; entry dropped"
        ),
    }
}

/// Unix time in milliseconds.
fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_append_as_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("audit.jsonl");
        let log = AuditLog::new(path.clone());

        log.append(&AuditEntry::now("c1", "terminal", Outcome::Ok, None));
        log.append(&AuditEntry::now(
            "c2",
            "mouse",
            Outcome::Refused,
            Some("capability disabled".to_string()),
        ));

        let text = std::fs::read_to_string(log.path()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);

        let e1: AuditEntry = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(e1.corr_id, "c1");
        assert_eq!(e1.outcome, Outcome::Ok);
        assert_eq!(e1.error, None);

        let e2: AuditEntry = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(e2.outcome, Outcome::Refused);
        assert_eq!(e2.error.as_deref(), Some("capability disabled"));
    }

    #[test]
    fn to_value_round_trips() {
        let e = AuditEntry::now("c", "server", Outcome::Error, Some("boom".into()));
        let v = e.to_value();
        assert_eq!(v["capability"], "server");
        assert_eq!(v["outcome"], "error");
        assert_eq!(v["error"], "boom");
    }
}
