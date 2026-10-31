//! Web Push: VAPID keys, payload encryption, delivery, and the Assistant's
//! "something new happened" notifier.
//!
//! - **Encryption**: RFC 8291 (`aes128gcm`, one record): ECDH with the
//!   browser's `p256dh` key, HKDF-SHA-256 keyed by its `auth` secret.
//! - **Auth**: RFC 8292 VAPID — an ES256 JWT for the endpoint's origin,
//!   signed with the server's persisted key pair.
//! - **Delivery**: one POST per subscription (RFC 8030 `TTL` / `Urgency`),
//!   no redirects, short timeouts. `404` / `410` mean the browser dropped
//!   the subscription, so it is deleted.
//! - **Endpoints**: https on a known push service only (see
//!   [`validate_endpoint`]) — the server POSTs to whatever URL a
//!   subscription names, so it must not be a way to reach arbitrary hosts.
//! - **Notifier**: when the voice gate delivers a relay turn into the
//!   Assistant and nobody has the Assistant panel open, push "Assistant"
//!   with a short summary — at most one per [`PUSH_MIN_INTERVAL`]; relays
//!   inside the window coalesce into the next one.

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use hkdf::Hkdf;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{PublicKey, SecretKey};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::time::Instant;

use crate::db::Db;
use crate::restart_resume::{NO_RESUME_ENV, resume_disabled};
use crate::service::voice_gate::{Held, RelayKind};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// RFC 8188 record size advertised in the header. The whole payload is one
/// record, so this only has to exceed it.
const RECORD_SIZE: u32 = 4096;
/// Largest plaintext that fits one record (16-byte tag, 1 delimiter byte).
const MAX_PLAINTEXT: usize = RECORD_SIZE as usize - 17;
/// `sub` claim of the VAPID JWT: a contact URL for the push service.
const VAPID_SUBJECT: &str = "https://peckboard.com";
/// VAPID JWT lifetime (RFC 8292 caps it at 24 h).
const VAPID_TTL_SECS: i64 = 12 * 60 * 60;
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Push-service host suffixes a subscription endpoint may point at (FCM for
/// Chrome/Edge/Android, Mozilla autopush, Apple, Windows WNS).
const PUSH_SERVICE_HOSTS: &[&str] = &[
    "fcm.googleapis.com",
    "android.googleapis.com",
    "push.services.mozilla.com",
    "push.apple.com",
    "notify.windows.com",
];

/// At most one Assistant push per this interval.
pub const PUSH_MIN_INTERVAL: Duration = Duration::from_secs(30);
/// A relay notification older than this is stale: the push service drops it.
const ASSISTANT_TTL_SECS: u32 = 60 * 60;
const BODY_CHARS: usize = 180;
/// Summaries held for one coalesced push; older ones are dropped.
const PENDING_CAP: usize = 20;
/// Deliver even with [`NO_RESUME_ENV`] set (a scratch run pointed at a
/// deliberately subscribed browser).
pub const ALLOW_UNDER_NO_RESUME_ENV: &str = "PECKBOARD_PUSH_ALLOW_UNDER_NO_RESUME";

/// VAPID push notification service.
#[derive(Debug, Clone)]
pub struct PushService {
    pub vapid_public_key: String,
    pub vapid_private_key: String,
}

/// A client push subscription.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushSubscription {
    pub endpoint: String,
    pub keys: PushSubscriptionKeys,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushSubscriptionKeys {
    pub p256dh: String,
    pub auth: String,
}

impl From<&crate::db::models::PushSubscription> for PushSubscription {
    fn from(row: &crate::db::models::PushSubscription) -> Self {
        PushSubscription {
            endpoint: row.endpoint.clone(),
            keys: PushSubscriptionKeys {
                p256dh: row.p256dh.clone(),
                auth: row.auth_key.clone(),
            },
        }
    }
}

/// What the service worker shows (`web/public/sw.js`).
#[derive(Debug, Clone, Serialize)]
pub struct PushMessage {
    pub title: String,
    pub body: String,
    pub url: String,
    /// Same tag = the browser replaces the previous notification.
    pub tag: String,
}

/// Result of one delivery attempt that reached the push service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    Delivered,
    /// `404` / `410`: the subscription is gone and should be deleted.
    Gone,
}

/// Persisted VAPID key pair on disk.
#[derive(Serialize, Deserialize)]
struct VapidKeysFile {
    public_key: String,
    private_key: String,
}

/// Generate a fresh VAPID key pair.
///
/// Returns `(public_b64url, private_b64url)` where the public key is
/// the uncompressed P-256 SEC1 encoding (0x04 prefix + 64 bytes of
/// affine coordinates) derived from the private scalar via proper EC
/// point multiplication, and the private key is the 32-byte scalar.
///
/// The previous version filled the public-key buffer with random bytes
/// and the key pair was therefore cryptographically meaningless: any
/// push notification signed with the private key would have failed
/// signature verification against the stored public key. This version
/// uses the `p256` crate so the pair is actually usable.
pub fn generate_vapid_keys() -> (String, String) {
    let secret = SecretKey::random(&mut rand::thread_rng());
    let private_bytes = secret.to_bytes();
    let public_point = secret.public_key().to_encoded_point(false);

    let private_b64 = B64.encode(private_bytes.as_slice());
    let public_b64 = B64.encode(public_point.as_bytes());

    (public_b64, private_b64)
}

/// Load VAPID keys from `<data_dir>/vapid_keys.json`, generating and
/// persisting a new pair if the file does not yet exist.
///
/// The file is created with mode 0o600 on Unix via `OpenOptions` so
/// the secret never lands on disk world-readable, even briefly.
pub fn load_or_create_vapid_keys(data_dir: &Path) -> Result<(String, String)> {
    let path = data_dir.join("vapid_keys.json");

    if path.exists() {
        let contents = std::fs::read_to_string(&path)?;
        let keys: VapidKeysFile = serde_json::from_str(&contents)?;
        tracing::info!("Loaded VAPID keys from {}", path.display());
        return Ok((keys.public_key, keys.private_key));
    }

    let (public_key, private_key) = generate_vapid_keys();

    std::fs::create_dir_all(data_dir)?;
    let file = VapidKeysFile {
        public_key: public_key.clone(),
        private_key: private_key.clone(),
    };
    let json = serde_json::to_string_pretty(&file)?;
    write_secret_file(&path, json.as_bytes())?;
    tracing::info!("Generated and saved VAPID keys to {}", path.display());

    Ok((public_key, private_key))
}

/// Write `data` to `path` so that on Unix the file is created with mode
/// 0o600 atomically — never world-readable even briefly. On non-Unix
/// platforms it falls back to a plain write.
#[cfg(unix)]
fn write_secret_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(data)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_secret_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, data)
}

/// Browsers hand out keys as base64url; accept padding and the standard
/// alphabet too.
fn b64_decode(s: &str) -> Result<Vec<u8>> {
    let s: String = s
        .trim()
        .trim_end_matches('=')
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            c => c,
        })
        .collect();
    B64.decode(s).context("invalid base64")
}

/// The endpoint a subscription may be delivered to: https, no userinfo,
/// default port, on a known push service.
pub fn validate_endpoint(raw: &str) -> std::result::Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(raw).map_err(|_| "endpoint is not a URL".to_string())?;
    if url.scheme() != "https" {
        return Err("endpoint must be https".into());
    }
    if !url.username().is_empty() || url.password().is_some() || url.port().is_some() {
        return Err("endpoint must not carry credentials or a port".into());
    }
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let known = PUSH_SERVICE_HOSTS
        .iter()
        .any(|s| host == *s || host.ends_with(&format!(".{s}")));
    if !known {
        return Err(format!("{host} is not a known push service"));
    }
    Ok(url)
}

/// Check a subscription's keys are usable (a P-256 point, a 16-byte secret).
pub fn validate_keys(p256dh: &str, auth: &str) -> std::result::Result<(), String> {
    let point = b64_decode(p256dh).map_err(|_| "p256dh is not base64".to_string())?;
    PublicKey::from_sec1_bytes(&point).map_err(|_| "p256dh is not a P-256 key".to_string())?;
    let auth = b64_decode(auth).map_err(|_| "auth is not base64".to_string())?;
    if auth.len() != 16 {
        return Err("auth must be 16 bytes".into());
    }
    Ok(())
}

/// RFC 8291 `aes128gcm` body for `plaintext`, with a fresh ephemeral key
/// and salt.
pub fn encrypt(p256dh: &str, auth: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
    let mut salt = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut salt);
    let ephemeral = SecretKey::random(&mut rand::thread_rng());
    encrypt_with(
        &b64_decode(p256dh)?,
        &b64_decode(auth)?,
        plaintext,
        &ephemeral,
        &salt,
    )
}

/// [`encrypt`] with the sender key and salt injected (RFC 8291 §5 test
/// vector).
fn encrypt_with(
    ua_public: &[u8],
    auth_secret: &[u8],
    plaintext: &[u8],
    as_secret: &SecretKey,
    salt: &[u8; 16],
) -> Result<Vec<u8>> {
    if auth_secret.len() != 16 {
        bail!("auth secret must be 16 bytes");
    }
    if plaintext.len() > MAX_PLAINTEXT {
        bail!("push payload too large ({} bytes)", plaintext.len());
    }
    let ua_key =
        PublicKey::from_sec1_bytes(ua_public).map_err(|_| anyhow!("invalid p256dh key"))?;
    let ua_point = ua_key.to_encoded_point(false);
    let as_point = as_secret.public_key().to_encoded_point(false);
    let shared = p256::ecdh::diffie_hellman(as_secret.to_nonzero_scalar(), ua_key.as_affine());

    // IKM = HKDF(auth_secret, ecdh_secret, "WebPush: info" 0x00 ua_public as_public)
    let mut key_info = Vec::with_capacity(14 + 65 + 65);
    key_info.extend_from_slice(b"WebPush: info\0");
    key_info.extend_from_slice(ua_point.as_bytes());
    key_info.extend_from_slice(as_point.as_bytes());
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(auth_secret), shared.raw_secret_bytes().as_slice())
        .expand(&key_info, &mut ikm)
        .map_err(|_| anyhow!("hkdf expand failed"))?;

    let prk = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let mut cek = [0u8; 16];
    prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
        .map_err(|_| anyhow!("hkdf expand failed"))?;
    let mut nonce = [0u8; 12];
    prk.expand(b"Content-Encoding: nonce\0", &mut nonce)
        .map_err(|_| anyhow!("hkdf expand failed"))?;

    // One (last) record: plaintext, 0x02 delimiter, no padding.
    let mut record = Vec::with_capacity(plaintext.len() + 1);
    record.extend_from_slice(plaintext);
    record.push(0x02);
    let cipher = Aes128Gcm::new_from_slice(&cek).map_err(|_| anyhow!("invalid key length"))?;
    let nonce = Nonce::try_from(nonce.as_slice()).map_err(|_| anyhow!("invalid nonce length"))?;
    let ciphertext = cipher
        .encrypt(&nonce, record.as_slice())
        .map_err(|_| anyhow!("encryption failed"))?;

    // Header: salt(16) rs(4) idlen(1) keyid(as_public), then the record.
    let keyid = as_point.as_bytes();
    let mut body = Vec::with_capacity(21 + keyid.len() + ciphertext.len());
    body.extend_from_slice(salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(keyid.len() as u8);
    body.extend_from_slice(keyid);
    body.extend_from_slice(&ciphertext);
    Ok(body)
}

/// RFC 8292 VAPID JWT (ES256) for `aud`, expiring at `exp` (unix seconds).
fn vapid_jwt(private_b64: &str, aud: &str, exp: i64) -> Result<String> {
    let key = SigningKey::from_slice(&b64_decode(private_b64)?)
        .map_err(|_| anyhow!("invalid VAPID private key"))?;
    let header = B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
    let claims = B64.encode(serde_json::to_vec(&serde_json::json!({
        "aud": aud,
        "exp": exp,
        "sub": VAPID_SUBJECT,
    }))?);
    let input = format!("{header}.{claims}");
    let sig: Signature = key.sign(input.as_bytes());
    Ok(format!("{input}.{}", B64.encode(sig.to_bytes())))
}

/// The HTTP client push deliveries go through: bounded, never redirected.
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(SEND_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

impl PushService {
    /// Create a new PushService, loading or generating VAPID keys
    /// persisted under `data_dir`.
    pub fn new(data_dir: &Path) -> Self {
        let (public_key, private_key) = load_or_create_vapid_keys(data_dir).unwrap_or_else(|e| {
            tracing::warn!("Failed to load/create VAPID keys: {e}, using ephemeral keys");
            generate_vapid_keys()
        });
        PushService {
            vapid_public_key: public_key,
            vapid_private_key: private_key,
        }
    }

    /// Encrypt `payload` for `sub` and POST it to its push service.
    /// `urgency` is an RFC 8030 value (`very-low` / `low` / `normal` /
    /// `high`). `Err` = not delivered for a reason other than the
    /// subscription being gone.
    pub async fn send(
        &self,
        http: &reqwest::Client,
        sub: &PushSubscription,
        payload: &[u8],
        ttl_secs: u32,
        urgency: &str,
    ) -> Result<SendOutcome> {
        let url = validate_endpoint(&sub.endpoint).map_err(|e| anyhow!(e))?;
        let body = encrypt(&sub.keys.p256dh, &sub.keys.auth, payload)?;
        let aud = url.origin().ascii_serialization();
        let jwt = vapid_jwt(
            &self.vapid_private_key,
            &aud,
            chrono::Utc::now().timestamp() + VAPID_TTL_SECS,
        )?;
        let resp = http
            .post(url)
            .header("TTL", ttl_secs.to_string())
            .header("Urgency", urgency)
            .header("Content-Encoding", "aes128gcm")
            .header("Content-Type", "application/octet-stream")
            .header(
                "Authorization",
                format!("vapid t={jwt}, k={}", self.vapid_public_key),
            )
            .body(body)
            .send()
            .await
            .context("push request failed")?;
        let status = resp.status();
        if status.is_success() {
            return Ok(SendOutcome::Delivered);
        }
        if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE {
            return Ok(SendOutcome::Gone);
        }
        let text = resp.text().await.unwrap_or_default();
        let text: String = text.chars().take(200).collect();
        bail!("push service answered {status}: {text}")
    }

    /// Send `msg` to every stored subscription, deleting the gone ones.
    /// Returns `(delivered, failed)`.
    pub async fn send_to_all(
        &self,
        db: &Db,
        http: &reqwest::Client,
        msg: &PushMessage,
        ttl_secs: u32,
    ) -> (usize, usize) {
        let subs = match db.list_push_subscriptions().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("push: listing subscriptions failed: {e}");
                return (0, 0);
            }
        };
        let Ok(payload) = serde_json::to_vec(msg) else {
            return (0, subs.len());
        };
        let (mut delivered, mut failed) = (0, 0);
        for row in &subs {
            let sub = PushSubscription::from(row);
            match self.send(http, &sub, &payload, ttl_secs, "normal").await {
                Ok(SendOutcome::Delivered) => delivered += 1,
                Ok(SendOutcome::Gone) => {
                    failed += 1;
                    tracing::info!(endpoint = %row.endpoint, "push: subscription gone, deleting");
                    if let Err(e) = db.delete_push_subscription(&row.endpoint).await {
                        tracing::warn!("push: deleting gone subscription failed: {e}");
                    }
                }
                Err(e) => {
                    failed += 1;
                    tracing::warn!(endpoint = %row.endpoint, "push: delivery failed: {e:#}");
                }
            }
        }
        (delivered, failed)
    }
}

// ── Assistant notifier ──────────────────────────────────────────────────

/// `PECKBOARD_NO_RESUME=1` marks a scratch or copied data dir whose stored
/// subscriptions may be the user's real browsers: push nothing unless
/// [`ALLOW_UNDER_NO_RESUME_ENV`] is `1`.
pub fn delivery_suppressed() -> bool {
    let var = |k: &str| std::env::var(k).ok();
    resume_disabled(var(NO_RESUME_ENV).as_deref())
        && var(ALLOW_UNDER_NO_RESUME_ENV).as_deref() != Some("1")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    /// The Assistant panel is open: the user sees the turn already.
    Skip,
    SendNow,
    /// Within [`PUSH_MIN_INTERVAL`] of the last push: hold for a coalesced
    /// push at this instant (`None` = a flush is already scheduled).
    Hold(Option<Instant>),
}

fn decide(
    watched: bool,
    last_sent: Option<Instant>,
    flush_scheduled: bool,
    now: Instant,
) -> Decision {
    if watched {
        return Decision::Skip;
    }
    if flush_scheduled {
        return Decision::Hold(None);
    }
    match last_sent {
        Some(t) if now.saturating_duration_since(t) < PUSH_MIN_INTERVAL => {
            Decision::Hold(Some(t + PUSH_MIN_INTERVAL))
        }
        _ => Decision::SendNow,
    }
}

/// One notification line per relay. Session relays name the session only —
/// their content may be private and shows on a lock screen.
fn relay_line(h: &Held) -> String {
    match h.kind {
        RelayKind::Scheduled => format!("Scheduled task: {}", h.summary),
        RelayKind::Question => format!("{} has a question", h.source_name),
        RelayKind::Update => format!("Update from {}", h.source_name),
    }
}

fn cut(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let head: String = s.chars().take(max).collect();
        format!("{head}\u{2026}")
    } else {
        s.to_string()
    }
}

fn coalesced_body(lines: &[String]) -> String {
    let body = match lines {
        [] => "New activity".to_string(),
        [one] => one.clone(),
        many => format!("{} updates: {}", many.len(), many.join(" \u{00b7} ")),
    };
    cut(&body, BODY_CHARS)
}

#[derive(Default)]
struct NotifyState {
    last_sent: Option<Instant>,
    pending: Vec<String>,
    flush_scheduled: bool,
}

type Watched = Box<dyn Fn() -> bool + Send + Sync>;

pub struct AssistantNotifier {
    suppressed: bool,
    db: Db,
    push: PushService,
    http: reqwest::Client,
    watched: Watched,
    state: Mutex<NotifyState>,
}

static NOTIFIER: OnceLock<Arc<AssistantNotifier>> = OnceLock::new();

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Install the process-wide Assistant notifier (boot; later calls are
/// no-ops). `watched` says whether an Assistant panel is visible.
pub fn install_assistant_notifier(
    db: Db,
    push: PushService,
    watched: impl Fn() -> bool + Send + Sync + 'static,
) {
    let suppressed = delivery_suppressed();
    if suppressed {
        tracing::info!(
            "{NO_RESUME_ENV} set: Assistant web push is off (set {ALLOW_UNDER_NO_RESUME_ENV}=1 to override)"
        );
    }
    let _ = NOTIFIER.set(Arc::new(AssistantNotifier {
        suppressed,
        db,
        push,
        http: http_client(),
        watched: Box::new(watched),
        state: Mutex::new(NotifyState::default()),
    }));
}

/// The voice gate just delivered `items` as a relay turn into the
/// Assistant. Push a notification unless the panel is being watched.
/// Never blocks: delivery runs on its own task.
pub fn notify_assistant_relays(items: &[Held]) {
    let Some(n) = NOTIFIER.get() else {
        return;
    };
    if n.suppressed || items.is_empty() {
        return;
    }
    n.clone().note(items.iter().map(relay_line).collect());
}

impl AssistantNotifier {
    fn note(self: Arc<Self>, lines: Vec<String>) {
        let now = Instant::now();
        let watched = (self.watched)();
        let mut st = lock(&self.state);
        match decide(watched, st.last_sent, st.flush_scheduled, now) {
            Decision::Skip => {}
            Decision::SendNow => {
                st.last_sent = Some(now);
                drop(st);
                tokio::spawn(self.deliver(coalesced_body(&lines)));
            }
            Decision::Hold(at) => {
                st.pending.extend(lines);
                let over = st.pending.len().saturating_sub(PENDING_CAP);
                st.pending.drain(..over);
                if let Some(at) = at {
                    st.flush_scheduled = true;
                    drop(st);
                    tokio::spawn(self.flush_at(at));
                }
            }
        }
    }

    async fn flush_at(self: Arc<Self>, at: Instant) {
        tokio::time::sleep_until(at).await;
        let lines = {
            let mut st = lock(&self.state);
            st.flush_scheduled = false;
            let lines = std::mem::take(&mut st.pending);
            // Opened in the meantime: the user has seen it.
            if lines.is_empty() || (self.watched)() {
                return;
            }
            st.last_sent = Some(Instant::now());
            lines
        };
        self.deliver(coalesced_body(&lines)).await;
    }

    async fn deliver(self: Arc<Self>, body: String) {
        let msg = PushMessage {
            title: "Assistant".into(),
            body,
            url: "/assistant".into(),
            tag: "peckboard-assistant".into(),
        };
        let (delivered, failed) = self
            .push
            .send_to_all(&self.db, &self.http, &msg, ASSISTANT_TTL_SECS)
            .await;
        if delivered + failed > 0 {
            tracing::info!(delivered, failed, "push: Assistant notification sent");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::VerifyingKey;
    use p256::ecdsa::signature::Verifier;

    #[test]
    fn test_generate_vapid_keys() {
        let (public_key, private_key) = generate_vapid_keys();
        assert!(!public_key.is_empty());
        assert!(!private_key.is_empty());
        assert_ne!(public_key, private_key);
        let pub_bytes = B64.decode(&public_key).unwrap();
        assert_eq!(pub_bytes.len(), 65);
        assert_eq!(pub_bytes[0], 0x04);
        let priv_bytes = B64.decode(&private_key).unwrap();
        assert_eq!(priv_bytes.len(), 32);

        // The public key MUST be the EC point derived from the private
        // scalar — earlier versions filled it with random bytes, which
        // made push notifications cryptographically broken.
        let secret = p256::SecretKey::from_bytes(priv_bytes.as_slice().into()).unwrap();
        let derived = secret.public_key().to_encoded_point(false);
        assert_eq!(derived.as_bytes(), pub_bytes.as_slice());
    }

    #[cfg(unix)]
    #[test]
    fn test_vapid_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let _ = load_or_create_vapid_keys(tmp.path()).unwrap();
        let mode = std::fs::metadata(tmp.path().join("vapid_keys.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn test_push_service_new() {
        let tmp = tempfile::tempdir().unwrap();
        let service = PushService::new(tmp.path());
        assert!(!service.vapid_public_key.is_empty());
        assert!(!service.vapid_private_key.is_empty());
    }

    #[test]
    fn test_vapid_keys_persisted_and_reloaded() {
        let tmp = tempfile::tempdir().unwrap();
        let (pub1, priv1) = load_or_create_vapid_keys(tmp.path()).unwrap();
        let (pub2, priv2) = load_or_create_vapid_keys(tmp.path()).unwrap();
        assert_eq!(pub1, pub2);
        assert_eq!(priv1, priv2);
    }

    /// RFC 8291 §5 worked example, byte for byte — plus a decrypt with the
    /// receiver's private key, as a browser would.
    #[test]
    fn encryption_matches_rfc8291_and_decrypts() {
        let plaintext = b"When I grow up, I want to be a watermelon";
        let as_secret = SecretKey::from_slice(
            &b64_decode("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw").unwrap(),
        )
        .unwrap();
        let ua_private = SecretKey::from_slice(
            &b64_decode("q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94").unwrap(),
        )
        .unwrap();
        let ua_public = b64_decode(
            "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
        )
        .unwrap();
        let auth = b64_decode("BTBZMqHH6r4Tts7J_aSIgg").unwrap();
        let salt: [u8; 16] = b64_decode("DGv6ra1nlYgDCS1FRnbzlw")
            .unwrap()
            .try_into()
            .unwrap();

        let body = encrypt_with(&ua_public, &auth, plaintext, &as_secret, &salt).unwrap();
        assert_eq!(
            B64.encode(&body),
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
        );

        // Receiver side: parse the header, re-derive, decrypt.
        let (salt, rest) = body.split_at(16);
        let idlen = rest[4] as usize;
        let as_public = PublicKey::from_sec1_bytes(&rest[5..5 + idlen]).unwrap();
        let ciphertext = &rest[5 + idlen..];
        let shared =
            p256::ecdh::diffie_hellman(ua_private.to_nonzero_scalar(), as_public.as_affine());
        let mut info = b"WebPush: info\0".to_vec();
        info.extend_from_slice(&ua_public);
        info.extend_from_slice(as_public.to_encoded_point(false).as_bytes());
        let mut ikm = [0u8; 32];
        Hkdf::<Sha256>::new(Some(&auth), shared.raw_secret_bytes().as_slice())
            .expand(&info, &mut ikm)
            .unwrap();
        let prk = Hkdf::<Sha256>::new(Some(salt), &ikm);
        let (mut cek, mut nonce) = ([0u8; 16], [0u8; 12]);
        prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
            .unwrap();
        prk.expand(b"Content-Encoding: nonce\0", &mut nonce)
            .unwrap();
        let record = Aes128Gcm::new_from_slice(&cek)
            .unwrap()
            .decrypt(&Nonce::try_from(nonce.as_slice()).unwrap(), ciphertext)
            .unwrap();
        assert_eq!(record.last(), Some(&0x02));
        assert_eq!(&record[..record.len() - 1], plaintext);
    }

    #[test]
    fn vapid_jwt_verifies_against_the_public_key() {
        let (public, private) = generate_vapid_keys();
        let jwt = vapid_jwt(&private, "https://fcm.googleapis.com", 1_900_000_000).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let claims: serde_json::Value =
            serde_json::from_slice(&B64.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
        assert_eq!(claims["exp"], 1_900_000_000);
        let key = VerifyingKey::from_sec1_bytes(&B64.decode(&public).unwrap()).unwrap();
        let sig = Signature::from_slice(&B64.decode(parts[2]).unwrap()).unwrap();
        key.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
            .unwrap();
    }

    #[test]
    fn endpoints_must_be_https_push_services() {
        assert!(validate_endpoint("https://fcm.googleapis.com/fcm/send/abc").is_ok());
        assert!(validate_endpoint("https://web.push.apple.com/QGx").is_ok());
        assert!(validate_endpoint("http://fcm.googleapis.com/fcm/send/abc").is_err());
        assert!(validate_endpoint("https://127.0.0.1/push").is_err());
        assert!(validate_endpoint("https://evil.example/fcm.googleapis.com").is_err());
        assert!(validate_endpoint("https://fcm.googleapis.com.evil.example/x").is_err());
        assert!(validate_endpoint("https://fcm.googleapis.com:8443/x").is_err());
    }

    #[test]
    fn push_decision_skips_when_watched_and_rate_limits() {
        let now = Instant::now();
        // Panel visible → no push, whatever the history.
        assert_eq!(decide(true, None, false, now), Decision::Skip);
        assert_eq!(decide(true, Some(now), true, now), Decision::Skip);
        // Unwatched, nothing sent lately → send now.
        assert_eq!(decide(false, None, false, now), Decision::SendNow);
        let long_ago = now - PUSH_MIN_INTERVAL;
        assert_eq!(decide(false, Some(long_ago), false, now), Decision::SendNow);
        // Inside the window → hold until it closes; once a flush is
        // scheduled, later relays just join it.
        let recent = now - Duration::from_secs(10);
        assert_eq!(
            decide(false, Some(recent), false, now),
            Decision::Hold(Some(recent + PUSH_MIN_INTERVAL))
        );
        assert_eq!(decide(false, Some(recent), true, now), Decision::Hold(None));
    }

    #[test]
    fn coalesced_body_counts_and_caps() {
        assert_eq!(coalesced_body(&["a".into()]), "a");
        assert_eq!(
            coalesced_body(&["a".into(), "b".into()]),
            "2 updates: a \u{00b7} b"
        );
        let long = vec!["x".repeat(500)];
        assert_eq!(coalesced_body(&long).chars().count(), BODY_CHARS + 1);
    }
}
