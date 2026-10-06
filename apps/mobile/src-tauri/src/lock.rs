//! App lock: a code (4–8 digits) or a 3×3 pattern, optionally with
//! biometrics on top, guarding the shell and every box behind it.
//!
//! Where things live:
//!
//! | what                                   | where                             |
//! | -------------------------------------- | --------------------------------- |
//! | salted Argon2id hash of the secret     | secure storage, key `lock.v1`     |
//! | method, code length, options, backoff  | `lock.json` in the app data dir   |
//!
//! Secure storage is the source of truth for the lock's method; `lock.json`
//! caches it next to the options. A readable `lock.json` without a method
//! means no lock. A missing or unreadable one fails closed: with a box
//! paired the app starts locked and the lock is rebuilt from the stored
//! hash ([`LockManager::reconcile`]); with none paired — a reinstall, which
//! drops every pairing and is the recovery path for a forgotten code (the
//! iOS keychain survives it) — a stale hash is deleted. Secure storage
//! needs the main thread on iOS, so it's never read during boot itself,
//! only from a background task right after.
//!
//! The wait after failed attempts counts down on the monotonic clock and
//! is persisted as what's left of it, restarting in full on the next
//! launch: setting the wall clock forward can't skip it.
//!
//! While locked, [`LockManager::unlocked`] refuses to hand out the
//! [`Unlocked`] proof the tunnel's start / resume / pairing paths require,
//! and the shell's commands fail with [`LOCKED`] (`commands::ShellProof`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use serde::{Deserialize, Serialize};
use tauri_plugin_peckboard_native::BiometricKind;
use tokio::sync::{OwnedRwLockReadGuard, RwLock};

use crate::store::{SecretStore, write_atomic};

/// Event the shell listens to; payload is a [`LockStatus`].
pub const LOCK_EVENT: &str = "lock-status";
/// The error every non-lock command returns while the app is locked.
pub const LOCKED: &str = "locked";
/// Secure-storage key of the secret's hash.
pub const SECRET_KEY: &str = "lock.v1";
/// Reason shown in the system biometric prompt.
pub const BIOMETRIC_REASON: &str = "Unlock PeckBoard";
const TOO_MANY: &str = "Too many attempts. Try again later.";
const UNREADABLE: &str = "Couldn't read the app lock from secure storage. Try again.";
const MAX_BACKOFF_MS: u64 = 15 * 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LockMethod {
    Code,
    Pattern,
}

impl LockMethod {
    fn wrong(self) -> String {
        match self {
            Self::Code => "wrong code".into(),
            Self::Pattern => "wrong pattern".into(),
        }
    }
}

/// How long the app may be away (background / unfocused / asleep) before
/// coming back requires an unlock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutoLock {
    #[default]
    #[serde(rename = "immediate")]
    Immediate,
    #[serde(rename = "1m")]
    OneMinute,
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "15m")]
    FifteenMinutes,
}

impl AutoLock {
    fn after_ms(self) -> u64 {
        match self {
            Self::Immediate => 0,
            Self::OneMinute => 60_000,
            Self::FiveMinutes => 5 * 60_000,
            Self::FifteenMinutes => 15 * 60_000,
        }
    }
}

/// Wait imposed after the `failures`-th consecutive wrong secret: the first
/// four are free, then 30 s, 1 min, 5 min, and 15 min from the 8th on.
pub fn backoff_ms(failures: u32) -> u64 {
    match failures {
        0..=4 => 0,
        5 => 30_000,
        6 => 60_000,
        7 => 5 * 60_000,
        _ => MAX_BACKOFF_MS,
    }
}

/// Code: 4–8 ASCII digits. Pattern: dot indices 0–8 joined with `-`, at
/// least 4 dots, none repeated.
pub fn validate_secret(method: LockMethod, secret: &str) -> Result<(), &'static str> {
    match method {
        LockMethod::Code => {
            if (4..=8).contains(&secret.len()) && secret.bytes().all(|b| b.is_ascii_digit()) {
                Ok(())
            } else {
                Err("A code is 4 to 8 digits.")
            }
        }
        LockMethod::Pattern => {
            const BAD: &str = "A pattern connects at least 4 dots, each once.";
            let mut seen = [false; 9];
            let mut n = 0;
            for part in secret.split('-') {
                let &[d @ b'0'..=b'8'] = part.as_bytes() else {
                    return Err(BAD);
                };
                let i = usize::from(d - b'0');
                if seen[i] {
                    return Err(BAD);
                }
                seen[i] = true;
                n += 1;
            }
            if n >= 4 { Ok(()) } else { Err(BAD) }
        }
    }
}

/// What the shell renders (see the contract in `main.ts`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LockStatus {
    pub enabled: bool,
    pub locked: bool,
    pub method: Option<LockMethod>,
    pub code_length: Option<u8>,
    pub biometrics: bool,
    pub biometric_kind: BiometricKind,
    pub auto_lock: AutoLock,
    pub retry_after_ms: Option<u64>,
    pub failures: u32,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnlockResult {
    pub ok: bool,
    pub status: LockStatus,
}

/// `lock.json`: everything but the hash.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LockFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    method: Option<LockMethod>,
    #[serde(default)]
    code_length: Option<u8>,
    #[serde(default)]
    biometrics: bool,
    #[serde(default)]
    auto_lock: AutoLock,
    #[serde(default)]
    failures: u32,
    /// What was left of the wait after failed attempts when last saved
    /// (ms); restarts in full on the next launch.
    #[serde(default)]
    retry_wait_ms: Option<u64>,
}

/// The value stored under [`SECRET_KEY`].
#[derive(Serialize, Deserialize)]
struct SecretRecord {
    method: LockMethod,
    /// Argon2id PHC string (algorithm, parameters, salt, hash).
    hash: String,
}

struct Inner {
    file: LockFile,
    locked: bool,
    /// Wall-clock ms the app was last seen leaving the foreground.
    away_since: Option<u64>,
    /// No code / pattern attempt is accepted before this.
    retry_until: Option<Instant>,
    /// `file` has been checked against secure storage this launch.
    reconciled: bool,
    /// `lock.json` was missing or unreadable at launch.
    file_lost: bool,
}

/// Result of checking a code / pattern.
#[derive(Debug, PartialEq, Eq)]
enum Attempt {
    Ok,
    Wrong,
    /// Refused (not counted): still in backoff.
    Backoff,
    /// Not a well-formed secret for the method: can't match, not counted.
    Malformed,
}

// Proof token: bearer has checked that the app is not locked, and holds the
// lock gate's read side for as long as it lives, so the lock can't engage
// (and its tunnel stop run) between that check and the work done with the
// token. The only constructor is `LockManager::unlocked`. See
// `commands::connect_box` for an example.
pub struct Unlocked(#[allow(dead_code)] OwnedRwLockReadGuard<()>);

#[cfg(test)]
impl Unlocked {
    pub fn for_tests() -> Self {
        Self(Arc::new(RwLock::new(())).try_read_owned().unwrap())
    }
}

/// While alive, one of our own biometric prompts is showing: it takes the
/// window's focus / resigns the app active, which must not count as the
/// app being away. From [`LockManager::own_prompt`].
pub struct OwnPrompt<'a>(&'a AtomicUsize);

impl Drop for OwnPrompt<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The monotonic clock (swappable in tests).
type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

pub struct LockManager {
    path: PathBuf,
    secrets: Arc<dyn SecretStore>,
    params: Params,
    clock: Clock,
    /// A box was paired at launch (see [`Self::reconcile`]).
    boxes_paired: bool,
    inner: Mutex<Inner>,
    /// Serialises secret checks and every secure-storage write, so parallel
    /// attempts can't outrun the failure counter.
    attempts: Mutex<()>,
    /// Read side: held by every [`Unlocked`]. Write side: taken to lock.
    gate: Arc<RwLock<()>>,
    /// Our own biometric prompts in flight ([`OwnPrompt`]).
    prompts: AtomicUsize,
}

impl LockManager {
    /// Load `lock.json` at `path`. With a lock configured — or the file lost
    /// while a box is paired — the app starts locked.
    pub fn load(
        path: impl Into<PathBuf>,
        secrets: Arc<dyn SecretStore>,
        boxes_paired: bool,
    ) -> Self {
        Self::with_params(
            path,
            secrets,
            Params::default(),
            boxes_paired,
            Arc::new(Instant::now),
        )
    }

    fn with_params(
        path: impl Into<PathBuf>,
        secrets: Arc<dyn SecretStore>,
        params: Params,
        boxes_paired: bool,
        clock: Clock,
    ) -> Self {
        let path = path.into();
        let file = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice::<LockFile>(&b)
                .inspect_err(|e| log::warn!("{} unreadable: {e}", path.display()))
                .ok(),
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    log::warn!("{} unreadable: {e}", path.display());
                }
                None
            }
        };
        let file_lost = file.is_none();
        let file = file.unwrap_or_default();
        // Fail closed: a lost lock.json may have held a lock; until secure
        // storage says otherwise, the paired boxes stay behind it.
        let locked = file.method.is_some() || (file_lost && boxes_paired);
        let retry_until = file
            .retry_wait_ms
            .map(|w| clock() + Duration::from_millis(w.min(MAX_BACKOFF_MS)));
        Self {
            path,
            secrets,
            params,
            clock,
            boxes_paired,
            inner: Mutex::new(Inner {
                reconciled: !file_lost && file.method.is_none(),
                file,
                locked,
                away_since: None,
                retry_until,
                file_lost,
            }),
            attempts: Mutex::new(()),
            gate: Arc::new(RwLock::new(())),
            prompts: AtomicUsize::new(0),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.lock().unwrap().file.method.is_some()
    }

    pub fn is_locked(&self) -> bool {
        self.inner.lock().unwrap().locked
    }

    pub fn biometrics_on(&self) -> bool {
        let i = self.inner.lock().unwrap();
        i.file.method.is_some() && i.file.biometrics
    }

    pub fn status(&self, biometric_kind: BiometricKind) -> LockStatus {
        let i = self.inner.lock().unwrap();
        let f = &i.file;
        let enabled = f.method.is_some();
        LockStatus {
            enabled,
            locked: i.locked,
            method: f.method,
            code_length: f.code_length.filter(|_| f.method == Some(LockMethod::Code)),
            biometrics: enabled && f.biometrics,
            biometric_kind,
            auto_lock: f.auto_lock,
            retry_after_ms: retry_after(i.retry_until, (self.clock)()),
            failures: f.failures,
        }
    }

    /// The proof the tunnel needs, or [`LOCKED`].
    pub async fn unlocked(&self) -> Result<Unlocked, String> {
        let guard = self.gate.clone().read_owned().await;
        if self.is_locked() {
            return Err(LOCKED.into());
        }
        Ok(Unlocked(guard))
    }

    /// Lock now (if a lock is configured). Waits for in-flight [`Unlocked`]
    /// work (a tunnel start / resume, a pairing round's launch) to finish
    /// first, so the caller's tunnel stop afterwards catches it. True if
    /// the app is locked.
    pub async fn engage(&self) -> bool {
        let _w = self.gate.write().await;
        let mut i = self.inner.lock().unwrap();
        i.away_since = None;
        if i.file.method.is_none() {
            return false;
        }
        i.locked = true;
        true
    }

    /// Mark one of our own biometric prompts as showing until the guard
    /// drops: [`Self::note_away`] ignores the focus / active loss it causes.
    pub fn own_prompt(&self) -> OwnPrompt<'_> {
        self.prompts.fetch_add(1, Ordering::SeqCst);
        OwnPrompt(&self.prompts)
    }

    pub fn prompt_in_flight(&self) -> bool {
        self.prompts.load(Ordering::SeqCst) > 0
    }

    /// The app left the foreground at `at_ms` (earliest one wins until
    /// [`Self::clear_away`]). Ignored while our own prompt is showing.
    pub fn note_away(&self, at_ms: u64) {
        if self.prompt_in_flight() {
            return;
        }
        let mut i = self.inner.lock().unwrap();
        i.away_since = Some(i.away_since.map_or(at_ms, |t| t.min(at_ms)));
    }

    pub fn clear_away(&self) {
        self.inner.lock().unwrap().away_since = None;
    }

    /// Back in the foreground at `now_ms`: has the app been away long
    /// enough for the lock to engage? A clock that went backwards counts as
    /// due.
    pub fn due(&self, now_ms: u64) -> bool {
        let i = self.inner.lock().unwrap();
        if i.file.method.is_none() || i.locked {
            return false;
        }
        match i.away_since {
            None => false,
            Some(t) => now_ms < t || now_ms - t >= i.file.auto_lock.after_ms(),
        }
    }

    /// Bring `lock.json` in line with secure storage, once per launch (a
    /// read error leaves it for the next call). The stored record's method
    /// wins over `lock.json`'s; a hash with `lock.json` lost rebuilds the
    /// lock if a box is paired, else is deleted. A missing hash never turns
    /// a lock off. True if the lock's state changed. Blocking (reads secure
    /// storage): never on the main thread.
    pub fn reconcile(&self) -> Result<bool, String> {
        let _a = self.attempts.lock().unwrap();
        self.reconcile_locked()
    }

    /// [`Self::reconcile`]; caller holds `attempts`.
    fn reconcile_locked(&self) -> Result<bool, String> {
        if self.inner.lock().unwrap().reconciled {
            return Ok(false);
        }
        let stored = match self.secrets.get(SECRET_KEY) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("app lock: secure storage unreadable: {e:#}");
                return Err(UNREADABLE.into());
            }
        };
        let record = match stored.map(|s| serde_json::from_str::<SecretRecord>(&s)) {
            None => None,
            Some(Ok(r)) => Some(r.method),
            Some(Err(e)) => {
                log::warn!("app lock: stored record unreadable: {e}");
                return Err("The stored app lock is unreadable.".into());
            }
        };
        let mut i = self.inner.lock().unwrap();
        if i.reconciled {
            return Ok(false);
        }
        let before = (i.locked, i.file.method);
        let mut save = std::mem::take(&mut i.file_lost);
        let mut stale = false;
        match (i.file.method, record) {
            (Some(m), Some(r)) if m != r => {
                log::warn!("app lock: lock.json said {m:?}, secure storage {r:?}");
                i.file.method = Some(r);
                i.file.code_length = None;
                save = true;
            }
            (Some(_), Some(_)) => {}
            // Fail closed: stays on (and locked if it was); attempts error.
            (Some(_), None) => log::warn!("app lock: hash missing from secure storage"),
            (None, Some(r)) if self.boxes_paired => {
                log::warn!("app lock: lock.json lost; restoring the lock from secure storage");
                i.file = LockFile {
                    version: 1,
                    method: Some(r),
                    ..LockFile::default()
                };
                i.locked = true;
                save = true;
            }
            (None, Some(_)) => {
                log::info!("app lock: no box paired; deleting a stale hash");
                stale = true;
                i.locked = false;
            }
            (None, None) => i.locked = false,
        }
        i.reconciled = true;
        if save {
            self.save_or_log(&i);
        }
        let changed = before != (i.locked, i.file.method);
        drop(i);
        if stale && let Err(e) = self.secrets.delete(SECRET_KEY) {
            log::warn!("stale app lock hash not deleted: {e:#}");
        }
        Ok(changed)
    }

    /// Configure a lock (only when none is). Leaves the app unlocked.
    pub fn setup(
        &self,
        method: LockMethod,
        secret: &str,
        auto_lock: AutoLock,
    ) -> Result<(), String> {
        let _a = self.attempts.lock().unwrap();
        if self.is_enabled() {
            return Err("App lock is already on.".into());
        }
        validate_secret(method, secret)?;
        self.store_secret(method, secret)?;
        let mut i = self.inner.lock().unwrap();
        i.file = LockFile {
            version: 1,
            method: Some(method),
            code_length: code_length(method, secret),
            biometrics: false,
            auto_lock,
            failures: 0,
            retry_wait_ms: None,
        };
        i.retry_until = None;
        i.locked = false;
        i.reconciled = true;
        i.file_lost = false;
        self.save_or_log(&i);
        Ok(())
    }

    /// Unlock with the code / pattern. `Ok(false)`: wrong, malformed, or in
    /// backoff (only a wrong one counts).
    pub fn unlock(&self, secret: &str) -> Result<bool, String> {
        let _a = self.attempts.lock().unwrap();
        let ok = self.attempt(secret)? == Attempt::Ok;
        if ok {
            let mut i = self.inner.lock().unwrap();
            i.locked = false;
            i.away_since = None;
        }
        Ok(ok)
    }

    /// A biometric prompt succeeded: unlock and reset the failure count.
    pub fn unlock_biometric(&self) {
        let mut i = self.inner.lock().unwrap();
        if i.file.method.is_none() {
            return;
        }
        i.locked = false;
        i.away_since = None;
        if i.file.failures != 0 || i.retry_until.is_some() {
            i.file.failures = 0;
            i.retry_until = None;
            self.save_or_log(&i);
        }
    }

    /// Check the existing code / pattern before a settings change: errors
    /// `"wrong code"` / `"wrong pattern"` (counted), or a backoff refusal.
    pub fn check_current(&self, current: &str) -> Result<(), String> {
        let _a = self.attempts.lock().unwrap();
        self.check_current_locked(current)
    }

    fn check_current_locked(&self, current: &str) -> Result<(), String> {
        match self.attempt(current)? {
            Attempt::Ok => Ok(()),
            Attempt::Backoff => Err(TOO_MANY.into()),
            Attempt::Wrong | Attempt::Malformed => Err(self.wrong()),
        }
    }

    /// Replace the secret (and maybe the method), given the current one.
    pub fn change(&self, current: &str, method: LockMethod, secret: &str) -> Result<(), String> {
        let _a = self.attempts.lock().unwrap();
        validate_secret(method, secret)?;
        self.check_current_locked(current)?;
        // In an order no failure can strand the user in: drop the code
        // length first (the shell copes without it), then swap the hash,
        // then record the new method — and should that last save fail,
        // `reconcile` takes the method from secure storage next launch.
        let old_length = {
            let mut i = self.inner.lock().unwrap();
            let old = i.file.code_length.take();
            if let Err(e) = self.save(&i) {
                i.file.code_length = old;
                return Err(format!("Couldn't save the app lock settings: {e:#}"));
            }
            old
        };
        if let Err(e) = self.store_secret(method, secret) {
            let mut i = self.inner.lock().unwrap();
            i.file.code_length = old_length;
            self.save_or_log(&i);
            return Err(e);
        }
        let mut i = self.inner.lock().unwrap();
        i.file.method = Some(method);
        i.file.code_length = code_length(method, secret);
        self.save_or_log(&i);
        Ok(())
    }

    /// Change options; the caller has authorised it.
    pub fn set_options(
        &self,
        biometrics: Option<bool>,
        auto_lock: Option<AutoLock>,
    ) -> Result<(), String> {
        let mut i = self.inner.lock().unwrap();
        if i.file.method.is_none() {
            return Err("App lock is off.".into());
        }
        if let Some(b) = biometrics {
            i.file.biometrics = b;
        }
        if let Some(a) = auto_lock {
            i.file.auto_lock = a;
        }
        self.save_or_log(&i);
        Ok(())
    }

    /// Turn the lock off; the caller has authorised it. `lock.json` first:
    /// should that fail the lock stays on, rather than coming back next
    /// launch.
    pub fn disable(&self) -> Result<(), String> {
        let _a = self.attempts.lock().unwrap();
        let mut i = self.inner.lock().unwrap();
        let old = (std::mem::take(&mut i.file), i.retry_until.take());
        if let Err(e) = self.save(&i) {
            (i.file, i.retry_until) = old;
            return Err(format!("Couldn't turn the app lock off: {e:#}"));
        }
        i.locked = false;
        i.away_since = None;
        drop(i);
        if let Err(e) = self.secrets.delete(SECRET_KEY) {
            log::warn!("app lock off, but its hash wasn't deleted: {e:#}");
        }
        Ok(())
    }

    fn wrong(&self) -> String {
        self.inner
            .lock()
            .unwrap()
            .file
            .method
            .map_or_else(|| "App lock is off.".into(), LockMethod::wrong)
    }

    /// One code / pattern check. Caller holds `attempts`.
    fn attempt(&self, secret: &str) -> Result<Attempt, String> {
        self.reconcile_locked()?;
        let method = {
            let i = self.inner.lock().unwrap();
            let Some(method) = i.file.method else {
                return Err("App lock is off.".into());
            };
            if retry_after(i.retry_until, (self.clock)()).is_some() {
                return Ok(Attempt::Backoff);
            }
            method
        };
        if validate_secret(method, secret).is_err() {
            return Ok(Attempt::Malformed);
        }
        let stored = self.secrets.get(SECRET_KEY).map_err(|e| {
            log::warn!("app lock: secure storage unreadable: {e:#}");
            UNREADABLE.to_string()
        })?;
        // Fail closed: a hash secure storage can't produce never turns the
        // lock off, and isn't counted.
        let Some(stored) = stored else {
            log::warn!("app lock: hash missing from secure storage");
            return Err(UNREADABLE.into());
        };
        let ok = serde_json::from_str::<SecretRecord>(&stored)
            .context("unreadable record")
            .and_then(|r| verify(&r.hash, secret))
            .map_err(|e| format!("The stored app lock is unreadable: {e:#}"))?;
        let mut i = self.inner.lock().unwrap();
        if ok {
            if i.file.failures != 0 || i.retry_until.is_some() {
                i.file.failures = 0;
                i.retry_until = None;
                self.save_or_log(&i);
            }
            return Ok(Attempt::Ok);
        }
        i.file.failures = i.file.failures.saturating_add(1);
        let wait = backoff_ms(i.file.failures);
        i.retry_until = (wait > 0).then(|| (self.clock)() + Duration::from_millis(wait));
        self.save_or_log(&i);
        Ok(Attempt::Wrong)
    }

    fn store_secret(&self, method: LockMethod, secret: &str) -> Result<(), String> {
        let hash = hash_secret(&self.params, secret).map_err(|e| format!("{e:#}"))?;
        let rec =
            serde_json::to_string(&SecretRecord { method, hash }).map_err(|e| e.to_string())?;
        self.secrets
            .set(SECRET_KEY, &rec)
            .map_err(|e| format!("Couldn't save the app lock to secure storage: {e:#}"))
    }

    /// Persist `lock.json` (with what's left of the backoff wait).
    fn save(&self, i: &Inner) -> anyhow::Result<()> {
        let mut file = i.file.clone();
        file.retry_wait_ms = retry_after(i.retry_until, (self.clock)());
        let bytes = serde_json::to_vec_pretty(&file)?;
        write_atomic(&self.path, &bytes)
    }

    /// [`Self::save`]; on failure the in-memory state still applies this
    /// launch.
    fn save_or_log(&self, i: &Inner) {
        if let Err(e) = self.save(i) {
            log::warn!("saving {} failed: {e:#}", self.path.display());
        }
    }
}

/// Ms left until `until`, if any.
fn retry_after(until: Option<Instant>, now: Instant) -> Option<u64> {
    until
        .map(|u| u.saturating_duration_since(now).as_millis() as u64)
        .filter(|&ms| ms > 0)
}

fn code_length(method: LockMethod, secret: &str) -> Option<u8> {
    (method == LockMethod::Code).then_some(secret.len() as u8)
}

fn argon(params: &Params) -> Argon2<'static> {
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params.clone())
}

fn hash_secret(params: &Params, secret: &str) -> anyhow::Result<String> {
    use rand::RngCore;
    let mut salt = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    let salt = SaltString::encode_b64(&salt).map_err(|e| anyhow!("salt: {e}"))?;
    let hash = argon(params)
        .hash_password(secret.as_bytes(), &salt)
        .map_err(|e| anyhow!("hashing the app lock failed: {e}"))?;
    Ok(hash.to_string())
}

/// Constant-time check against a PHC string (its own parameters apply).
fn verify(hash: &str, secret: &str) -> anyhow::Result<bool> {
    let parsed = PasswordHash::new(hash).map_err(|e| anyhow!("{e}"))?;
    Ok(Argon2::default()
        .verify_password(secret.as_bytes(), &parsed)
        .is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::MemSecrets;
    use std::sync::atomic::AtomicU64;

    const T0: u64 = 1_700_000_000_000;

    /// A monotonic clock the test moves by hand (ms since its start).
    #[derive(Clone)]
    struct FakeClock(Instant, Arc<AtomicU64>);

    impl FakeClock {
        fn new() -> Self {
            Self(Instant::now(), Arc::default())
        }
        fn advance(&self, ms: u64) {
            self.1.fetch_add(ms, Ordering::SeqCst);
        }
        fn clock(&self) -> Clock {
            let c = self.clone();
            Arc::new(move || c.0 + Duration::from_millis(c.1.load(Ordering::SeqCst)))
        }
    }

    fn mgr_with(
        dir: &tempfile::TempDir,
        secrets: &Arc<MemSecrets>,
        boxes_paired: bool,
        clock: &FakeClock,
    ) -> LockManager {
        // Cheap parameters: debug-build Argon2 with the defaults is slow.
        LockManager::with_params(
            dir.path().join("lock.json"),
            secrets.clone(),
            Params::new(8, 1, 1, None).unwrap(),
            boxes_paired,
            clock.clock(),
        )
    }

    fn mgr(dir: &tempfile::TempDir, secrets: &Arc<MemSecrets>) -> LockManager {
        mgr_with(dir, secrets, true, &FakeClock::new())
    }

    fn st(m: &LockManager) -> LockStatus {
        m.status(BiometricKind::None)
    }

    #[test]
    fn hash_round_trip_and_storage_split() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Arc::new(MemSecrets::default());
        let m = mgr(&dir, &secrets);
        // lock.json missing with a box paired: locked until secure storage
        // has been read and has no lock in it.
        assert!(!m.is_enabled() && m.is_locked());
        assert_eq!(m.reconcile(), Ok(true));
        assert!(!m.is_enabled() && !m.is_locked());
        m.setup(LockMethod::Code, "123456", AutoLock::OneMinute)
            .unwrap();
        assert!(!m.is_locked(), "setup leaves the app unlocked");
        let s = st(&m);
        assert_eq!(s.method, Some(LockMethod::Code));
        assert_eq!(s.code_length, Some(6));
        assert_eq!(s.auto_lock, AutoLock::OneMinute);
        assert!(
            m.setup(LockMethod::Code, "1111", AutoLock::Immediate)
                .is_err()
        );

        // The hash is in secure storage, salted Argon2id; never the secret
        // and never in lock.json.
        let rec = secrets.get(SECRET_KEY).unwrap().unwrap();
        assert!(
            rec.contains("$argon2id$") && !rec.contains("123456"),
            "{rec}"
        );
        let json = std::fs::read_to_string(dir.path().join("lock.json")).unwrap();
        assert!(
            !json.contains("argon") && !json.contains("123456"),
            "{json}"
        );

        // A reload starts locked; the right code unlocks.
        let m = mgr(&dir, &secrets);
        assert!(m.is_locked());
        assert_eq!(m.unlock("123456"), Ok(true));
        assert!(!m.is_locked());

        // Same secret, different salt.
        let p = Params::new(8, 1, 1, None).unwrap();
        assert_ne!(
            hash_secret(&p, "1234").unwrap(),
            hash_secret(&p, "1234").unwrap()
        );

        // Change to a pattern, then disable.
        assert_eq!(
            m.change("000000", LockMethod::Pattern, "0-1-2-5"),
            Err("wrong code".into())
        );
        m.change("123456", LockMethod::Pattern, "0-1-2-5").unwrap();
        assert_eq!(st(&m).code_length, None);
        assert_eq!(m.check_current("0-1-2-4"), Err("wrong pattern".into()));
        m.check_current("0-1-2-5").unwrap();
        m.disable().unwrap();
        assert!(!m.is_enabled() && secrets.get(SECRET_KEY).unwrap().is_none());
        assert!(!mgr(&dir, &secrets).is_locked());
    }

    #[test]
    fn a_missing_hash_never_turns_the_lock_off() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Arc::new(MemSecrets::default());
        let m = mgr(&dir, &secrets);
        m.setup(LockMethod::Code, "2468", AutoLock::Immediate)
            .unwrap();
        secrets.0.lock().unwrap().clear();
        let m = mgr(&dir, &secrets);
        assert_eq!(m.reconcile(), Ok(false));
        assert_eq!(m.unlock("2468"), Err(UNREADABLE.into()));
        assert_eq!(m.unlock("2468"), Err(UNREADABLE.into()));
        let s = st(&m);
        assert!(s.enabled && s.locked && s.failures == 0, "{s:?}");
        assert!(mgr(&dir, &secrets).is_locked());
    }

    #[test]
    fn a_lost_lock_json_is_rebuilt_from_secure_storage() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Arc::new(MemSecrets::default());
        let path = dir.path().join("lock.json");
        let m = mgr(&dir, &secrets);
        m.setup(LockMethod::Code, "2468", AutoLock::Immediate)
            .unwrap();

        // Unreadable, with a box paired: locked, and rebuilt.
        std::fs::write(&path, b"{not json").unwrap();
        let m = mgr(&dir, &secrets);
        assert!(m.is_locked());
        assert_eq!(m.reconcile(), Ok(true));
        let s = st(&m);
        assert!(s.enabled && s.locked, "{s:?}");
        assert_eq!((s.method, s.code_length), (Some(LockMethod::Code), None));
        assert_eq!(m.unlock("2468"), Ok(true));
        assert!(mgr(&dir, &secrets).is_locked(), "lock.json rewritten");

        // Missing, no box paired (a reinstall): the stale hash goes.
        std::fs::remove_file(&path).unwrap();
        let m = mgr_with(&dir, &secrets, false, &FakeClock::new());
        assert!(!m.is_locked());
        assert_eq!(m.reconcile(), Ok(false));
        assert!(!m.is_enabled() && secrets.get(SECRET_KEY).unwrap().is_none());
        assert!(!mgr(&dir, &secrets).is_locked(), "\"no lock\" written down");
    }

    #[test]
    fn secure_storage_method_wins_over_lock_json() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Arc::new(MemSecrets::default());
        let path = dir.path().join("lock.json");
        let m = mgr(&dir, &secrets);
        m.setup(LockMethod::Code, "2468", AutoLock::Immediate)
            .unwrap();
        let code_json = std::fs::read(&path).unwrap();
        m.change("2468", LockMethod::Pattern, "0-4-8-7").unwrap();
        // The last lock.json save of the change was lost.
        std::fs::write(&path, code_json).unwrap();
        let m = mgr(&dir, &secrets);
        assert_eq!(st(&m).method, Some(LockMethod::Code));
        assert_eq!(m.reconcile(), Ok(true));
        assert_eq!(st(&m).method, Some(LockMethod::Pattern));
        assert_eq!(m.unlock("0-4-8-7"), Ok(true));
    }

    #[test]
    fn wrong_secrets_count_and_back_off() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Arc::new(MemSecrets::default());
        let clock = FakeClock::new();
        let m = mgr_with(&dir, &secrets, true, &clock);
        m.setup(LockMethod::Code, "2468", AutoLock::Immediate)
            .unwrap();
        for n in 1..=4 {
            assert_eq!(m.unlock("0000"), Ok(false));
            let s = st(&m);
            assert_eq!((s.failures, s.retry_after_ms), (n, None));
        }
        // Malformed input never counts.
        assert_eq!(m.unlock("12"), Ok(false));
        assert_eq!(st(&m).failures, 4);
        for (n, wait) in [
            (5, 30_000),
            (6, 60_000),
            (7, 300_000),
            (8, 900_000),
            (9, 900_000),
        ] {
            assert_eq!(m.unlock("0000"), Ok(false));
            let s = st(&m);
            assert_eq!((s.failures, s.retry_after_ms), (n, Some(wait)));
            // In backoff even the right code is refused, and not counted.
            clock.advance(wait - 1);
            assert_eq!(m.unlock("2468"), Ok(false));
            assert_eq!(m.check_current("2468"), Err(TOO_MANY.into()));
            assert_eq!(st(&m).failures, n);
            clock.advance(1);
        }
        assert_eq!(m.unlock("2468"), Ok(true));
        let s = st(&m);
        assert_eq!((s.failures, s.retry_after_ms), (0, None));
        assert_eq!(
            [0, 4, 5, 6, 7, 8, 100].map(backoff_ms),
            [0, 0, 30_000, 60_000, 300_000, 900_000, 900_000]
        );
    }

    #[test]
    fn backoff_survives_a_restart_and_ignores_the_wall_clock() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Arc::new(MemSecrets::default());
        let clock = FakeClock::new();
        let m = mgr_with(&dir, &secrets, true, &clock);
        m.setup(LockMethod::Pattern, "0-3-6-7-8", AutoLock::Immediate)
            .unwrap();
        for _ in 0..5 {
            assert_eq!(m.unlock("0-1-2-3"), Ok(false));
        }
        // What's persisted is the wait left, not a wall-clock deadline: a
        // clock set forward (or a restart) can't skip it.
        let json = std::fs::read_to_string(dir.path().join("lock.json")).unwrap();
        assert!(json.contains("\"retryWaitMs\": 30000"), "{json}");
        clock.advance(10_000);
        assert_eq!(st(&m).retry_after_ms, Some(20_000));

        // A restart restarts the full persisted wait.
        let clock = FakeClock::new();
        let m = mgr_with(&dir, &secrets, true, &clock);
        let s = st(&m);
        assert_eq!((s.failures, s.retry_after_ms), (5, Some(30_000)));
        clock.advance(29_999);
        assert_eq!(m.unlock("0-3-6-7-8"), Ok(false));
        clock.advance(1);
        assert_eq!(m.unlock("0-3-6-7-8"), Ok(true));

        // A tampered-with wait is capped at the longest step.
        std::fs::write(
            dir.path().join("lock.json"),
            r#"{"version":1,"method":"pattern","failures":9,"retryWaitMs":86400000}"#,
        )
        .unwrap();
        let m = mgr(&dir, &secrets);
        assert_eq!(st(&m).retry_after_ms, Some(MAX_BACKOFF_MS));
    }

    #[test]
    fn auto_lock_is_due_after_the_configured_time_away() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Arc::new(MemSecrets::default());
        let m = mgr_with(&dir, &secrets, false, &FakeClock::new());
        // No lock: never due.
        m.note_away(T0);
        assert!(!m.due(T0 + 3_600_000));
        m.clear_away();

        m.setup(LockMethod::Code, "1234", AutoLock::FiveMinutes)
            .unwrap();
        assert!(!m.due(T0), "never away");
        m.note_away(T0);
        m.note_away(T0 + 60_000); // earliest wins
        assert!(!m.due(T0 + 299_999));
        assert!(m.due(T0 + 300_000));
        assert!(m.due(T0 - 1), "clock went backwards");
        m.clear_away();
        assert!(!m.due(T0 + 300_000));

        m.set_options(None, Some(AutoLock::Immediate)).unwrap();
        // Our own biometric prompt taking focus is not being away.
        {
            let _p = m.own_prompt();
            assert!(m.prompt_in_flight());
            m.note_away(T0);
        }
        assert!(!m.prompt_in_flight());
        assert!(!m.due(T0));
        m.note_away(T0);
        assert!(m.due(T0));

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            assert!(m.unlocked().await.is_ok());
            assert!(m.engage().await);
            assert!(m.is_locked());
            assert!(!m.due(T0 + 1), "already locked");
            assert_eq!(m.unlocked().await.err().as_deref(), Some(LOCKED));
            m.unlock("1234").unwrap();
            assert!(m.unlocked().await.is_ok());
            m.disable().unwrap();
            assert!(!m.engage().await, "nothing to lock with");
        });
    }

    #[test]
    fn secrets_are_validated() {
        use LockMethod::{Code, Pattern};
        for ok in ["1234", "00000000", "987654"] {
            assert_eq!(validate_secret(Code, ok), Ok(()), "{ok}");
        }
        for bad in ["123", "123456789", "12a4", "", "１２３４", "12 34"] {
            assert!(validate_secret(Code, bad).is_err(), "{bad}");
        }
        for ok in ["0-1-2-3", "8-7-6-5-4-3-2-1-0", "4-0-8-2"] {
            assert_eq!(validate_secret(Pattern, ok), Ok(()), "{ok}");
        }
        for bad in [
            "0-1-2", "0-1-2-1", "0-1-2-9", "0-1-2-", "0--1-2-3", "01-2-3", "0,1,2,3", "",
            "-1-2-3-4",
        ] {
            assert!(validate_secret(Pattern, bad).is_err(), "{bad}");
        }
        let dir = tempfile::tempdir().unwrap();
        let m = mgr(&dir, &Arc::new(MemSecrets::default()));
        assert!(m.setup(Pattern, "0-1-2", AutoLock::Immediate).is_err());
        assert!(!m.is_enabled());
    }
}
