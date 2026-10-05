//! Registered box identities ([`crate::identity`]), persisted under the
//! relay's `--state-dir` as [`REGISTRY_FILE`].
//!
//! File format, one entry per line (blank lines and `#` comments ignored):
//!
//! ```text
//! <base64url ed25519 public key> <registered_at, unix seconds>
//! ```
//!
//! Writes are atomic (temp file + rename, mode 0600) and every
//! read-modify-write — this process's or the admin CLI's — holds an
//! exclusive `flock` on [`LOCK_FILE`] from the read to the rename, so a
//! `registry revoke` can't be undone by a racing registration.
//!
//! Lookups ([`Registry::contains`], [`Snapshot`]) never touch disk: they
//! read the in-memory set. The relay re-checks the file's identity
//! (mtime/size/inode) on a background task every [`DEFAULT_RELOAD_INTERVAL`]
//! ([`Registry::reload`]), so an external edit or a revoke from the admin CLI
//! takes effect on a running relay within seconds, without a restart. A file
//! that fails to parse keeps the last good set (logged, retried); a missing
//! file is an empty registry.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::identity::{decode_key, encode_key, write_private};

/// File name under the state dir.
pub const REGISTRY_FILE: &str = "registered-boxes.txt";
/// Lock file next to it (the registry file itself is replaced by rename,
/// so it can't carry the lock).
pub const LOCK_FILE: &str = ".registered-boxes.lock";
/// How stale the in-memory view of the file may get on a running relay.
pub const DEFAULT_RELOAD_INTERVAL: Duration = Duration::from_secs(2);

const HEADER: &str =
    "# peckboard-relay registered boxes: <base64url ed25519 key> <registered_at unix secs>\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub key: [u8; 32],
    /// Unix seconds.
    pub registered_at: u64,
}

/// Outcome of [`Registry::add_capped`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Added {
    New,
    /// Already registered (idempotent; nothing written).
    Already,
    /// The registry holds the maximum number of keys; nothing written.
    Full,
}

/// (mtime, len, inode) of the file a view was loaded from.
type Stamp = (SystemTime, u64, u64);

#[derive(Default)]
struct State {
    keys: HashMap<[u8; 32], u64>,
    stamp: Option<Stamp>,
}

/// An immutable view of the registry at one moment: lookups take no lock
/// and never touch disk.
#[derive(Clone)]
pub struct Snapshot(Arc<State>);

impl Snapshot {
    pub fn contains(&self, key: &[u8; 32]) -> bool {
        self.0.keys.contains_key(key)
    }

    pub fn len(&self) -> usize {
        self.0.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.keys.is_empty()
    }
}

pub struct Registry {
    /// None: in-memory only (tests, or no state dir).
    path: Option<PathBuf>,
    reload_every: Duration,
    /// Swapped whole on every reload / write.
    state: RwLock<Arc<State>>,
    /// Serialises reloads and read-modify-writes within this process (the
    /// file lock covers other processes), so an older reload can never
    /// overwrite a newer write's view.
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

fn stamp_of(m: &std::fs::Metadata) -> std::io::Result<Stamp> {
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(m);
    #[cfg(not(unix))]
    let ino = 0;
    Ok((m.modified()?, m.len(), ino))
}

fn stamp(path: &Path) -> std::io::Result<Option<Stamp>> {
    match std::fs::metadata(path) {
        Ok(m) => Ok(Some(stamp_of(&m)?)),
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
            state: RwLock::new(Arc::default()),
            write: Mutex::new(()),
        }
    }

    /// How often the relay's background task calls [`reload`](Self::reload).
    pub fn with_reload_interval(mut self, every: Duration) -> Self {
        self.reload_every = every;
        self
    }

    pub fn reload_interval(&self) -> Duration {
        self.reload_every
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Re-read the file now if it changed (blocking IO: call it off the
    /// async workers). On any error the previous set stays in force.
    pub fn reload(&self) -> std::io::Result<()> {
        let _w = self.write.lock().unwrap();
        self.reload_locked()
    }

    /// Caller holds `write`.
    fn reload_locked(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        // Stamp and content from the same open file: a rename racing this
        // read can't pair new content with an old stamp or vice versa.
        let mut f = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if self.state.read().unwrap().stamp.is_some() {
                    *self.state.write().unwrap() = Arc::default();
                }
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let st = stamp_of(&f.metadata()?)?;
        if self.state.read().unwrap().stamp == Some(st) {
            return Ok(());
        }
        let mut text = String::new();
        f.read_to_string(&mut text)?;
        let keys = parse(&text).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{}: {e}", path.display()),
            )
        })?;
        *self.state.write().unwrap() = Arc::new(State {
            keys,
            stamp: Some(st),
        });
        Ok(())
    }

    /// The current set, for many lookups without touching the registry's
    /// lock again (e.g. while holding another lock).
    pub fn snapshot(&self) -> Snapshot {
        Snapshot(self.state.read().unwrap().clone())
    }

    /// Is `key` registered? In memory only; never touches disk.
    pub fn contains(&self, key: &[u8; 32]) -> bool {
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

    /// Register `key` with no cap (admin CLI). Ok(false) if it already
    /// was (idempotent).
    pub fn add(&self, key: [u8; 32]) -> std::io::Result<bool> {
        Ok(self.add_capped(key, usize::MAX)? == Added::New)
    }

    /// Register `key` unless the registry already holds `max_keys` keys.
    pub fn add_capped(&self, key: [u8; 32], max_keys: usize) -> std::io::Result<Added> {
        self.modify(|keys| {
            if keys.contains_key(&key) {
                (false, Added::Already)
            } else if keys.len() >= max_keys {
                (false, Added::Full)
            } else {
                keys.insert(key, now_secs());
                (true, Added::New)
            }
        })
    }

    /// Unregister `key`. Ok(false) if it wasn't registered.
    pub fn revoke(&self, key: &[u8; 32]) -> std::io::Result<bool> {
        self.modify(|keys| {
            let gone = keys.remove(key).is_some();
            (gone, gone)
        })
    }

    /// Exclusive lock shared with every other process editing this
    /// registry; released on drop. None for an in-memory registry.
    fn lock_file(&self) -> std::io::Result<Option<File>> {
        let Some(path) = &self.path else {
            return Ok(None);
        };
        let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(d) = dir {
            std::fs::create_dir_all(d)?;
        }
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let f = opts.open(path.with_file_name(LOCK_FILE))?;
        f.lock()?;
        Ok(Some(f))
    }

    /// Read-modify-write against the file's current content, under the
    /// file lock from read to rename (so edits by other processes are
    /// never lost or undone). `f` returns (changed, result).
    fn modify<T>(
        &self,
        f: impl FnOnce(&mut HashMap<[u8; 32], u64>) -> (bool, T),
    ) -> std::io::Result<T> {
        let _w = self.write.lock().unwrap();
        let _lock = self.lock_file()?;
        self.reload_locked()?;
        let mut keys = self.state.read().unwrap().keys.clone();
        let (changed, out) = f(&mut keys);
        if !changed {
            return Ok(out);
        }
        let st = match &self.path {
            Some(p) => {
                write_private(p, render(&keys).as_bytes())?;
                stamp(p)?
            }
            None => None,
        };
        *self.state.write().unwrap() = Arc::new(State { keys, stamp: st });
        Ok(out)
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
        let mut names: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![LOCK_FILE.to_string(), REGISTRY_FILE.to_string()]
        );
        let r2 = Registry::open_in(&d).unwrap();
        assert_eq!(r2.list().len(), 2);
        assert!(r2.list().iter().all(|e| e.registered_at > 0));

        // An external revoke (another process: the admin CLI) shows up on
        // the next reload without touching `r`; lookups alone never read.
        assert!(r2.revoke(&a).unwrap());
        assert!(!r2.revoke(&a).unwrap());
        assert!(r.contains(&a), "no disk IO on lookup");
        r.reload().unwrap();
        assert!(!r.contains(&a));
        assert!(r.contains(&b));

        // A corrupt file keeps the last good set.
        std::fs::write(d.join(REGISTRY_FILE), "not-a-key 1\nmore junk\n").unwrap();
        assert!(r.reload().is_err());
        assert!(r.contains(&b));
        // Deleted: empty.
        std::fs::remove_file(d.join(REGISTRY_FILE)).unwrap();
        r.reload().unwrap();
        assert!(!r.contains(&b));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn cap_refuses_new_keys_only() {
        let r = Registry::in_memory();
        let keys: Vec<_> = (0..3)
            .map(|_| BoxIdentity::generate().public_key())
            .collect();
        assert_eq!(r.add_capped(keys[0], 2).unwrap(), Added::New);
        assert_eq!(r.add_capped(keys[1], 2).unwrap(), Added::New);
        assert_eq!(r.add_capped(keys[2], 2).unwrap(), Added::Full);
        assert_eq!(r.add_capped(keys[0], 2).unwrap(), Added::Already);
        assert_eq!(r.len(), 2);
        let snap = r.snapshot();
        assert!(r.revoke(&keys[0]).unwrap());
        assert!(snap.contains(&keys[0]), "snapshots are immutable");
        assert!(!r.contains(&keys[0]));
        assert_eq!(r.add_capped(keys[2], 2).unwrap(), Added::New);
    }

    /// A revoke racing a stream of registrations from another handle (the
    /// CLI vs the running relay) always sticks.
    #[test]
    fn concurrent_revoke_and_add_keeps_the_revoke() {
        let d = tmp_dir();
        let victim = BoxIdentity::generate().public_key();
        Registry::open_in(&d).unwrap().add(victim).unwrap();
        for _ in 0..5 {
            let server = Arc::new(Registry::open_in(&d).unwrap());
            server.add(victim).unwrap();
            let adder = {
                let server = server.clone();
                std::thread::spawn(move || {
                    for _ in 0..40 {
                        server.add(BoxIdentity::generate().public_key()).unwrap();
                    }
                })
            };
            // A separate handle, like the admin CLI process.
            let cli = Registry::open_in(&d).unwrap();
            std::thread::sleep(Duration::from_millis(2));
            assert!(cli.revoke(&victim).unwrap());
            adder.join().unwrap();
            let fresh = Registry::open_in(&d).unwrap();
            assert!(!fresh.contains(&victim), "revoke undone by a racing add");
            server.reload().unwrap();
            assert!(!server.contains(&victim));
        }
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
