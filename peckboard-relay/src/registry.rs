//! Registered box identities ([`crate::identity`]), persisted under the
//! relay's `--state-dir` as [`REGISTRY_FILE`].
//!
//! File format, one entry per line (blank lines and `#` comments ignored):
//!
//! ```text
//! <base64url ed25519 public key> <registered_at, unix seconds>
//! ```
//!
//! Writes are atomic (temp file + rename, mode 0600). The relay re-checks
//! the file's mtime/size at most every [`DEFAULT_RELOAD_INTERVAL`], so an
//! external edit or a `registry revoke` from the admin CLI takes effect on
//! a running relay without a restart. A file that fails to parse keeps the
//! last good set (logged); a missing file is an empty registry.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tracing::warn;

use crate::identity::{decode_key, encode_key, write_private};

/// File name under the state dir.
pub const REGISTRY_FILE: &str = "registered-boxes.txt";
/// How stale the in-memory view of the file may get.
pub const DEFAULT_RELOAD_INTERVAL: Duration = Duration::from_secs(30);

const HEADER: &str =
    "# peckboard-relay registered boxes: <base64url ed25519 key> <registered_at unix secs>\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub key: [u8; 32],
    /// Unix seconds.
    pub registered_at: u64,
}

#[derive(Default)]
struct State {
    keys: HashMap<[u8; 32], u64>,
    /// (mtime, len) of the file this view was loaded from.
    stamp: Option<(SystemTime, u64)>,
}

pub struct Registry {
    /// None: in-memory only (tests, or no state dir).
    path: Option<PathBuf>,
    reload_every: Duration,
    state: RwLock<State>,
    epoch: Instant,
    /// Ms after `epoch` of the last mtime check.
    checked_ms: AtomicU64,
    /// Serialises read-modify-write of the file within this process.
    write: Mutex<()>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("path", &self.path)
            .field("len", &self.len())
            .finish()
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn stamp(path: &Path) -> std::io::Result<Option<(SystemTime, u64)>> {
    match std::fs::metadata(path) {
        Ok(m) => Ok(Some((m.modified()?, m.len()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Parse the file format (see the module docs). Errors name the line.
pub fn parse(text: &str) -> Result<HashMap<[u8; 32], u64>, String> {
    let mut keys = HashMap::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let key = parts
            .next()
            .and_then(decode_key)
            .ok_or_else(|| format!("line {}: bad key", i + 1))?;
        let at = match parts.next() {
            Some(t) => t
                .parse::<u64>()
                .map_err(|_| format!("line {}: bad timestamp", i + 1))?,
            None => 0,
        };
        keys.insert(key, at);
    }
    Ok(keys)
}

fn render(keys: &HashMap<[u8; 32], u64>) -> String {
    let mut entries: Vec<_> = keys.iter().collect();
    entries.sort_by_key(|(k, at)| (**at, **k));
    let mut out = String::from(HEADER);
    for (k, at) in entries {
        out.push_str(&format!("{} {at}\n", encode_key(k)));
    }
    out
}

impl Registry {
    /// An empty registry that never touches disk.
    pub fn in_memory() -> Self {
        Self::build(None)
    }

    /// The registry stored at `path` (missing: empty, created on first add).
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let r = Self::build(Some(path.into()));
        r.reload()?;
        Ok(r)
    }

    /// [`open`](Self::open) on `<state_dir>/`[`REGISTRY_FILE`].
    pub fn open_in(state_dir: &Path) -> std::io::Result<Self> {
        Self::open(state_dir.join(REGISTRY_FILE))
    }

    fn build(path: Option<PathBuf>) -> Self {
        Self {
            path,
            reload_every: DEFAULT_RELOAD_INTERVAL,
            state: RwLock::new(State::default()),
            epoch: Instant::now(),
            checked_ms: AtomicU64::new(0),
            write: Mutex::new(()),
        }
    }

    /// How often [`contains`](Self::contains) re-checks the file.
    pub fn with_reload_interval(mut self, every: Duration) -> Self {
        self.reload_every = every;
        self
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Re-read the file now if its mtime/size changed. On a parse error the
    /// previous set stays in force.
    pub fn reload(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        self.checked_ms
            .store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
        let st = stamp(path)?;
        if self.state.read().unwrap().stamp == st && st.is_some() {
            return Ok(());
        }
        let keys = match st {
            None => HashMap::new(),
            Some(_) => {
                let text = match std::fs::read_to_string(path) {
                    Ok(t) => t,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                    Err(e) => return Err(e),
                };
                parse(&text).map_err(|e| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("{}: {e}", path.display()),
                    )
                })?
            }
        };
        *self.state.write().unwrap() = State { keys, stamp: st };
        Ok(())
    }

    fn maybe_reload(&self) {
        if self.path.is_none() {
            return;
        }
        let now = self.epoch.elapsed().as_millis() as u64;
        let last = self.checked_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) < self.reload_every.as_millis() as u64 {
            return;
        }
        // One thread re-checks; the rest keep using the current view.
        if self
            .checked_ms
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        if let Err(e) = self.reload() {
            warn!("registry reload: {e}");
        }
    }

    /// Is `key` registered? Cheap; re-checks the file at most every
    /// reload interval.
    pub fn contains(&self, key: &[u8; 32]) -> bool {
        self.maybe_reload();
        self.state.read().unwrap().keys.contains_key(key)
    }

    pub fn len(&self) -> usize {
        self.state.read().unwrap().keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// All entries, oldest registration first.
    pub fn list(&self) -> Vec<Entry> {
        self.maybe_reload();
        let mut v: Vec<Entry> = self
            .state
            .read()
            .unwrap()
            .keys
            .iter()
            .map(|(k, at)| Entry {
                key: *k,
                registered_at: *at,
            })
            .collect();
        v.sort_by_key(|e| (e.registered_at, e.key));
        v
    }

    /// Register `key`. Ok(false) if it already was (idempotent).
    pub fn add(&self, key: [u8; 32]) -> std::io::Result<bool> {
        self.modify(|keys| {
            if keys.contains_key(&key) {
                return false;
            }
            keys.insert(key, now_secs());
            true
        })
    }

    /// Unregister `key`. Ok(false) if it wasn't registered.
    pub fn revoke(&self, key: &[u8; 32]) -> std::io::Result<bool> {
        self.modify(|keys| keys.remove(key).is_some())
    }

    /// Read-modify-write against the file's current content (so external
    /// edits made since the last reload are kept).
    fn modify(&self, f: impl FnOnce(&mut HashMap<[u8; 32], u64>) -> bool) -> std::io::Result<bool> {
        let _w = self.write.lock().unwrap();
        self.reload()?;
        let mut keys = self.state.read().unwrap().keys.clone();
        if !f(&mut keys) {
            return Ok(false);
        }
        let st = match &self.path {
            Some(p) => {
                write_private(p, render(&keys).as_bytes())?;
                stamp(p)?
            }
            None => None,
        };
        *self.state.write().unwrap() = State { keys, stamp: st };
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::BoxIdentity;

    fn tmp_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("peckrelay-reg-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn add_revoke_persist_and_reload() {
        let d = tmp_dir();
        let r = Registry::open_in(&d).unwrap();
        assert!(r.is_empty());
        let (a, b) = (
            BoxIdentity::generate().public_key(),
            BoxIdentity::generate().public_key(),
        );
        assert!(r.add(a).unwrap());
        assert!(!r.add(a).unwrap(), "idempotent");
        assert!(r.add(b).unwrap());
        assert!(r.contains(&a) && r.contains(&b));
        // Persisted atomically: no temp files left, readable by a fresh open.
        let names: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec![REGISTRY_FILE.to_string()]);
        let r2 = Registry::open_in(&d).unwrap();
        assert_eq!(r2.list().len(), 2);
        assert!(r2.list().iter().all(|e| e.registered_at > 0));

        // An external revoke (another process: the admin CLI) shows up
        // after the reload interval without touching `r`.
        let r = r.with_reload_interval(Duration::from_millis(0));
        assert!(r2.revoke(&a).unwrap());
        assert!(!r2.revoke(&a).unwrap());
        assert!(!r.contains(&a));
        assert!(r.contains(&b));

        // A corrupt file keeps the last good set.
        std::fs::write(d.join(REGISTRY_FILE), "not-a-key 1\nmore junk\n").unwrap();
        assert!(r.reload().is_err());
        assert!(r.contains(&b));
        // Deleted: empty.
        std::fs::remove_file(d.join(REGISTRY_FILE)).unwrap();
        assert!(!r.contains(&b));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn parse_tolerates_comments_and_missing_timestamp() {
        let k = BoxIdentity::generate().public_key();
        let text = format!("# hi\n\n  {} \n", encode_key(&k));
        assert_eq!(parse(&text).unwrap().get(&k), Some(&0));
        assert!(parse("abc 12\n").is_err());
    }
}
