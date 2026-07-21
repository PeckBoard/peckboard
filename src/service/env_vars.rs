//! Encryption + in-memory unlock layer for user-defined environment
//! variables. Storage lives in `db::crud::env_vars`; this module owns the
//! crypto and the runtime unlock state, and is the only place a plaintext
//! secret value is ever handled outside the DB row it decrypts from.
//!
//! Encryption: a per-var 16-byte Argon2id salt derives a 256-bit key from
//! the user's login password; AES-256-GCM with a fresh 12-byte nonce
//! encrypts the value (the GCM tag is appended to the ciphertext). A wrong
//! password surfaces as a GCM tag mismatch on decrypt — that mismatch *is*
//! the password check, so there is no separate verifier to leak against.
//!
//! Unlock state: [`EnvUnlockRegistry`] mirrors [`crate::service::askpass`] —
//! a per-request oneshot channel the UI answers, plus a short-lived cache of
//! decrypted values so a burst of session spawns doesn't re-prompt. Neither
//! the channel payloads nor the cache are ever persisted or logged.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::Argon2;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use rand::RngCore;
use rand::rngs::OsRng;
use tokio::sync::{Mutex, oneshot};

/// How long a user's decrypted values stay cached after an unlock before the
/// next use re-prompts. 30 minutes.
pub const UNLOCK_CACHE_TTL_SECS: u64 = 30 * 60;

/// How long a use-time unlock prompt waits for the user to answer before the
/// caller gives up. 2 minutes.
pub const UNLOCK_ANSWER_TIMEOUT_SECS: u64 = 120;

/// The decrypted contents of an unlock: var id → plaintext value. Keyed by
/// id because names are only unique per scope (global vs per-folder).
type ValueMap = HashMap<String, String>;

/// Ciphertext + the public inputs needed to reproduce the key and decrypt.
/// None of these fields is secret on its own (the password is what gates
/// decryption), so it is safe to persist / return them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptedValue {
    pub kdf_salt_hex: String,
    pub nonce_hex: String,
    pub ciphertext_b64: String,
}

/// Derive the 256-bit AES key from `password` and `salt` with Argon2id
/// (default params). Deterministic in both inputs, so decrypt reproduces the
/// exact key encrypt used.
fn derive_key(password: &str, salt: &[u8]) -> anyhow::Result<[u8; 32]> {
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|e| anyhow::anyhow!("key derivation failed: {e}"))?;
    Ok(key)
}

/// Encrypt `plaintext` under `password`. A fresh random salt and nonce are
/// drawn from the OS RNG on every call, so encrypting the same value twice
/// yields unrelated ciphertext.
pub fn encrypt_value(password: &str, plaintext: &str) -> anyhow::Result<EncryptedValue> {
    let mut salt = [0u8; 16];
    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut salt);
    OsRng.fill_bytes(&mut nonce_bytes);

    let key = derive_key(password, &salt)?;
    let cipher =
        Aes256Gcm::new_from_slice(&key).map_err(|_| anyhow::anyhow!("invalid key length"))?;
    let nonce = Nonce::try_from(nonce_bytes.as_slice())
        .map_err(|_| anyhow::anyhow!("invalid nonce length"))?;
    let ciphertext = cipher
        .encrypt(&nonce, plaintext.as_bytes())
        .map_err(|_| anyhow::anyhow!("encryption failed"))?;

    Ok(EncryptedValue {
        kdf_salt_hex: hex::encode(salt),
        nonce_hex: hex::encode(nonce_bytes),
        ciphertext_b64: B64.encode(ciphertext),
    })
}

/// Decrypt a value, returning `None` on ANY failure: malformed hex/base64, a
/// wrong-length nonce, or (the common case) a GCM tag mismatch from the wrong
/// password. Callers treat `None` as "password did not unlock this var".
pub fn decrypt_value(
    password: &str,
    kdf_salt_hex: &str,
    nonce_hex: &str,
    ciphertext_b64: &str,
) -> Option<String> {
    let salt = hex::decode(kdf_salt_hex).ok()?;
    let nonce_bytes = hex::decode(nonce_hex).ok()?;
    if nonce_bytes.len() != 12 {
        return None;
    }
    let ciphertext = B64.decode(ciphertext_b64).ok()?;

    let key = derive_key(password, &salt).ok()?;
    let cipher = Aes256Gcm::new_from_slice(&key).ok()?;
    let nonce = Nonce::try_from(nonce_bytes.as_slice()).ok()?;
    let plaintext = cipher.decrypt(&nonce, ciphertext.as_ref()).ok()?;
    String::from_utf8(plaintext).ok()
}

/// A use-time unlock prompt awaiting the user's answer.
struct Pending {
    user_id: String,
    /// The vars this request wants unlocked — carried for the UI prompt.
    #[allow(dead_code)]
    var_names: Vec<String>,
    tx: oneshot::Sender<Option<ValueMap>>,
}

/// A user's decrypted values, valid until `expires_at`.
struct CacheEntry {
    values: ValueMap,
    expires_at: Instant,
}

#[derive(Default)]
struct Inner {
    /// request_id → pending prompt.
    pending: HashMap<String, Pending>,
    /// user_id → cached decrypted values.
    cache: HashMap<String, CacheEntry>,
}

/// In-memory registry of in-flight unlock prompts and short-lived decrypted
/// value caches. Cheap to clone (shared `Arc` inner), modeled on
/// [`crate::service::askpass::AskpassRegistry`].
#[derive(Clone, Default)]
pub struct EnvUnlockRegistry {
    inner: Arc<Mutex<Inner>>,
}

impl EnvUnlockRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a use-time unlock prompt for `user_id` covering `var_names`.
    /// Returns the request id and a receiver that resolves when the prompt is
    /// answered (`Some(map)` = unlocked values, `None` = user cancelled) or
    /// errors if the request is dropped.
    pub async fn begin_request(
        &self,
        user_id: &str,
        var_names: Vec<String>,
    ) -> (String, oneshot::Receiver<Option<ValueMap>>) {
        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.inner.lock().await.pending.insert(
            request_id.clone(),
            Pending {
                user_id: user_id.to_string(),
                var_names,
                tx,
            },
        );
        (request_id, rx)
    }

    /// The user id a pending request belongs to, or `None` if unknown. Lets a
    /// route authorize the answerer without consuming the request.
    pub async fn pending_user(&self, request_id: &str) -> Option<String> {
        self.inner
            .lock()
            .await
            .pending
            .get(request_id)
            .map(|p| p.user_id.clone())
    }

    /// Deliver an answer to a pending request and remove it: `Some(map)` on
    /// success, `None` on cancel. Returns `false` for an unknown id. NOTE: a
    /// wrong-password attempt must NOT be routed here — the caller simply
    /// doesn't call `resolve`, leaving the request open for a retry.
    pub async fn resolve(&self, request_id: &str, decrypted: Option<ValueMap>) -> bool {
        let pending = self.inner.lock().await.pending.remove(request_id);
        match pending {
            Some(p) => p.tx.send(decrypted).is_ok(),
            None => false,
        }
    }

    /// Resolve EVERY pending request belonging to `user_id` with a clone of
    /// `values`. One submitted dialog answers all spawns waiting on the same
    /// owner — the client queues concurrent prompts behind a single dialog
    /// and never re-shows the later ones, so resolving only the answered
    /// request would leave the rest blocking until timeout. Returns how many
    /// requests were resolved.
    pub async fn resolve_all_for_user(&self, user_id: &str, values: &ValueMap) -> usize {
        let mut g = self.inner.lock().await;
        let ids: Vec<String> = g
            .pending
            .iter()
            .filter(|(_, p)| p.user_id == user_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &ids {
            if let Some(p) = g.pending.remove(id) {
                let _ = p.tx.send(Some(values.clone()));
            }
        }
        ids.len()
    }

    /// Drop a pending request without answering (timeout path). A late
    /// `resolve` for the same id then returns `false`.
    pub async fn drop_request(&self, request_id: &str) {
        self.inner.lock().await.pending.remove(request_id);
    }

    /// Cache a user's decrypted values for [`UNLOCK_CACHE_TTL_SECS`].
    pub async fn cache_put(&self, user_id: &str, values: ValueMap) {
        let expires_at = Instant::now() + Duration::from_secs(UNLOCK_CACHE_TTL_SECS);
        self.cache_put_at(user_id, values, expires_at).await;
    }

    /// Fetch a user's cached values, or `None` once expired. Purges every
    /// expired entry as a side effect.
    pub async fn cache_get(&self, user_id: &str) -> Option<ValueMap> {
        self.cache_get_at(user_id, Instant::now()).await
    }

    /// Drop a user's cached values (explicit lock, e.g. on logout).
    pub async fn lock_user(&self, user_id: &str) {
        self.inner.lock().await.cache.remove(user_id);
    }

    // ── time-injectable internals (used directly by tests) ────────────────

    async fn cache_put_at(&self, user_id: &str, values: ValueMap, expires_at: Instant) {
        self.inner
            .lock()
            .await
            .cache
            .insert(user_id.to_string(), CacheEntry { values, expires_at });
    }

    async fn cache_get_at(&self, user_id: &str, now: Instant) -> Option<ValueMap> {
        let mut g = self.inner.lock().await;
        g.cache.retain(|_, e| e.expires_at > now);
        g.cache.get(user_id).map(|e| e.values.clone())
    }

    /// Blocking snapshot of every owner's unexpired cached plaintexts,
    /// merged var id → value (ids are unique DB-wide, so a value appears
    /// at most once). Purges expired entries as a side effect. Uses
    /// `Mutex::blocking_lock` — callable only OUTSIDE the async runtime's
    /// worker threads (the blocking command-exec path qualifies).
    pub fn all_cached_values_blocking(&self) -> HashMap<String, String> {
        let now = Instant::now();
        let mut g = self.inner.blocking_lock();
        g.cache.retain(|_, e| e.expires_at > now);
        let mut out = HashMap::new();
        for entry in g.cache.values() {
            for (k, v) in &entry.values {
                out.insert(k.clone(), v.clone());
            }
        }
        out
    }
}

/// Process-global handle to the app's unlock registry, late-bound by `main`
/// (same pattern as `plugin::manager::set_notify_global`). Lets the
/// synchronous command-exec path snapshot unlocked values without threading
/// `AppState` into the plugin host. Unset in tests → snapshot is empty.
static GLOBAL_REGISTRY: OnceLock<Arc<EnvUnlockRegistry>> = OnceLock::new();

/// Bind the app's registry as the process-global one. First call wins.
pub fn set_global_registry(registry: Arc<EnvUnlockRegistry>) {
    let _ = GLOBAL_REGISTRY.set(registry);
}

/// The process-global registry, if bound. Exposed for exec-path tests that
/// seed the unlock cache the blocking snapshot reads.
#[cfg(test)]
pub(crate) fn global_registry() -> Option<Arc<EnvUnlockRegistry>> {
    GLOBAL_REGISTRY.get().cloned()
}

/// Every unlocked (cached) encrypted env var value, var id → plaintext, from
/// the process-global registry. Empty when no registry is bound or nothing
/// is unlocked. Blocking — call from a blocking thread only.
pub fn unlocked_values_blocking() -> HashMap<String, String> {
    match GLOBAL_REGISTRY.get() {
        Some(reg) => reg.all_cached_values_blocking(),
        None => HashMap::new(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let e = encrypt_value("hunter2", "my-secret-value").unwrap();
        assert_eq!(
            decrypt_value("hunter2", &e.kdf_salt_hex, &e.nonce_hex, &e.ciphertext_b64).as_deref(),
            Some("my-secret-value")
        );
    }

    #[test]
    fn wrong_password_returns_none() {
        let e = encrypt_value("hunter2", "secret").unwrap();
        assert!(
            decrypt_value("wrong-pw", &e.kdf_salt_hex, &e.nonce_hex, &e.ciphertext_b64).is_none()
        );
    }

    #[test]
    fn same_value_encrypts_differently_each_call() {
        let a = encrypt_value("pw", "same-value").unwrap();
        let b = encrypt_value("pw", "same-value").unwrap();
        assert_ne!(a.kdf_salt_hex, b.kdf_salt_hex);
        assert_ne!(a.nonce_hex, b.nonce_hex);
        assert_ne!(a.ciphertext_b64, b.ciphertext_b64);
        // Both still decrypt back to the original.
        assert_eq!(
            decrypt_value("pw", &a.kdf_salt_hex, &a.nonce_hex, &a.ciphertext_b64).as_deref(),
            Some("same-value")
        );
        assert_eq!(
            decrypt_value("pw", &b.kdf_salt_hex, &b.nonce_hex, &b.ciphertext_b64).as_deref(),
            Some("same-value")
        );
    }

    #[test]
    fn malformed_inputs_return_none() {
        assert!(decrypt_value("pw", "not-hex", "zz", "not-base64!!!").is_none());
    }

    #[tokio::test]
    async fn resolve_delivers_map_exactly_once() {
        let reg = EnvUnlockRegistry::new();
        let (id, rx) = reg.begin_request("u1", vec!["A".into()]).await;
        assert_eq!(reg.pending_user(&id).await.as_deref(), Some("u1"));

        let mut map = ValueMap::new();
        map.insert("A".into(), "secret".into());
        assert!(reg.resolve(&id, Some(map.clone())).await);
        assert_eq!(rx.await.unwrap(), Some(map));

        // Request is consumed: gone and un-resolvable a second time.
        assert!(reg.pending_user(&id).await.is_none());
        assert!(!reg.resolve(&id, None).await);
    }

    #[tokio::test]
    async fn cancel_delivers_none() {
        let reg = EnvUnlockRegistry::new();
        let (id, rx) = reg.begin_request("u1", vec![]).await;
        assert!(reg.resolve(&id, None).await);
        assert_eq!(rx.await.unwrap(), None);
    }

    #[tokio::test]
    async fn wrong_password_leaves_request_open() {
        let reg = EnvUnlockRegistry::new();
        let (id, _rx) = reg.begin_request("u1", vec![]).await;
        // A wrong-password attempt does NOT call resolve, so the request
        // survives for a retry.
        assert_eq!(reg.pending_user(&id).await.as_deref(), Some("u1"));
        assert!(reg.resolve(&id, Some(ValueMap::new())).await);
    }

    #[tokio::test]
    async fn resolve_all_for_user_answers_every_pending_request() {
        let reg = EnvUnlockRegistry::new();
        let (_id1, rx1) = reg.begin_request("u1", vec!["A".into()]).await;
        let (_id2, rx2) = reg.begin_request("u1", vec!["A".into()]).await;
        let (other_id, _rx3) = reg.begin_request("u2", vec!["B".into()]).await;

        let mut vals = ValueMap::new();
        vals.insert("A".into(), "secret".into());
        assert_eq!(reg.resolve_all_for_user("u1", &vals).await, 2);
        assert_eq!(rx1.await.unwrap(), Some(vals.clone()));
        assert_eq!(rx2.await.unwrap(), Some(vals));

        // The other user's request is untouched.
        assert_eq!(reg.pending_user(&other_id).await.as_deref(), Some("u2"));
    }

    #[tokio::test]
    async fn cache_expires_and_purges() {
        let reg = EnvUnlockRegistry::new();
        let now = Instant::now();
        let mut vals = ValueMap::new();
        vals.insert("A".into(), "1".into());

        // Already expired (expires_at == now, and the check is strictly `>`).
        reg.cache_put_at("u1", vals.clone(), now).await;
        assert!(reg.cache_get_at("u1", now).await.is_none());

        // Still valid.
        reg.cache_put_at("u1", vals.clone(), now + Duration::from_secs(60))
            .await;
        assert_eq!(reg.cache_get_at("u1", now).await, Some(vals));
    }

    #[tokio::test]
    async fn lock_user_clears_cache() {
        let reg = EnvUnlockRegistry::new();
        let mut vals = ValueMap::new();
        vals.insert("A".into(), "1".into());
        reg.cache_put("u1", vals.clone()).await;
        assert_eq!(reg.cache_get("u1").await, Some(vals));

        reg.lock_user("u1").await;
        assert!(reg.cache_get("u1").await.is_none());
    }
}
