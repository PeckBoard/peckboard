//! PeckBoard public skills hub: a small signed-publish server for
//! `SKILL.md` files, with forks and merge requests.
//!
//! - [`api`] — the HTTP surface for the catalog, publish and yank.
//! - [`collab`] — handles, skill info/history/diff, forks, merge requests.
//! - [`auth`] — Ed25519 keys, single-use nonces, proof of work, signed headers.
//! - [`git`] — one bare repo per skill and per fork (vendored libgit2).
//! - [`store`] — SQLite (WAL) metadata, yank/takedown flags, MRs.
//! - [`admin`] — the server-side admin CLI (ban, takedown, import, list).
//! - [`backup`] — nightly SQLite backups + repos tarballs.
//! - [`tls`] — ACME (TLS-ALPN-01) TLS serving.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub mod admin;
pub mod api;
pub mod auth;
pub mod backup;
pub mod collab;
pub mod git;
pub mod store;
pub mod tls;

/// Default leading-zero bits a never-seen key must solve before its first
/// signed request.
pub const DEFAULT_NEW_KEY_POW_BITS: u32 = 20;
/// Default successful publishes per key per UTC day.
pub const DEFAULT_PUBLISHES_PER_DAY: u32 = 20;
/// Largest `skill_md` accepted.
pub const MAX_SKILL_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone)]
pub struct Config {
    /// The public host name (certificate name, URL host).
    pub domain: String,
    /// Base of the URLs handed back to publishers (no trailing slash);
    /// `https://<domain>` unless overridden.
    pub public_url: String,
    pub data_dir: PathBuf,
    pub new_key_pow_bits: u32,
    pub publishes_per_day: u32,
}

impl Config {
    pub fn new(domain: &str, data_dir: PathBuf) -> Self {
        Config {
            domain: domain.to_string(),
            public_url: format!("https://{domain}"),
            data_dir,
            new_key_pow_bits: DEFAULT_NEW_KEY_POW_BITS,
            publishes_per_day: DEFAULT_PUBLISHES_PER_DAY,
        }
    }
}

/// `<data-dir>/repos`.
pub fn repos_dir(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("repos")
}

type IndexCache = Option<((i64, i64), store::RepoIndex)>;

/// The running hub: config, the store, the repos and the in-memory nonce
/// table.
pub struct Hub {
    pub cfg: Config,
    pub store: Mutex<store::Store>,
    pub repos: git::Repos,
    pub nonces: Mutex<auth::Nonces>,
    index_cache: Mutex<IndexCache>,
}

impl Hub {
    /// Open the data dir, moving any pre-git versions into repos first.
    pub fn open(cfg: Config) -> anyhow::Result<Arc<Hub>> {
        let mut store = store::Store::open(&cfg.data_dir)?;
        let repos = git::Repos::open(repos_dir(&cfg.data_dir))?;
        let n = store::migrate_to_git(&mut store, &repos)?;
        if n > 0 {
            tracing::info!("moved {n} stored version(s) into git");
        }
        Ok(Arc::new(Hub {
            cfg,
            store: Mutex::new(store),
            repos,
            nonces: Mutex::new(auth::Nonces::default()),
            index_cache: Mutex::new(None),
        }))
    }

    /// The store, even if a previous holder panicked (every write is a
    /// single statement or a transaction, so the connection stays sound).
    /// Take a repo lock *before* this, never while holding it.
    pub fn store(&self) -> std::sync::MutexGuard<'_, store::Store> {
        self.store.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn nonces(&self) -> std::sync::MutexGuard<'_, auth::Nonces> {
        self.nonces.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Start the nightly backup task on the current runtime.
    pub fn start_background(self: &Arc<Self>) {
        backup::spawn(self.clone());
    }

    /// `index.json`, rebuilt from SQLite + git only when the database
    /// changed (here or in the admin CLI) since the cached copy.
    pub fn index(&self) -> anyhow::Result<store::RepoIndex> {
        let store = self.store();
        let stamp = store.change_stamp()?;
        let mut cache = self.index_cache.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((s, idx)) = cache.as_ref()
            && *s == stamp
        {
            return Ok(idx.clone());
        }
        let idx = store.index(&self.repos)?;
        *cache = Some((stamp, idx.clone()));
        Ok(idx)
    }

    /// Drop the cached `index.json` (after a write).
    pub fn invalidate_index(&self) {
        *self.index_cache.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// `https://<domain>/skills/<name>/<version>/SKILL.md`.
    pub fn skill_url(&self, name: &str, version: &str) -> String {
        format!("{}/skills/{name}/{version}/SKILL.md", self.cfg.public_url)
    }
}

/// Now, RFC 3339 UTC to the second.
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Unix seconds → RFC 3339 UTC.
pub fn rfc3339_at(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Today's UTC date, `YYYY-MM-DD` (the publish rate-limit window).
pub fn today_utc() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}
