// VAPID web-push notifications

use std::path::Path;

use anyhow::Result;
use base64::Engine;
use p256::SecretKey;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use serde::{Deserialize, Serialize};

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
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;

    let secret = SecretKey::random(&mut rand::thread_rng());
    let private_bytes = secret.to_bytes();
    let public_point = secret.public_key().to_encoded_point(false);

    let private_b64 = engine.encode(private_bytes.as_slice());
    let public_b64 = engine.encode(public_point.as_bytes());

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
}

/// Send a push notification to a subscription (stub).
pub async fn send_push(subscription: &PushSubscription, title: &str, body: &str) -> Result<()> {
    tracing::info!(
        endpoint = %subscription.endpoint,
        title = %title,
        body = %body,
        "Push notification send attempted (stub)"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_vapid_keys() {
        let (public_key, private_key) = generate_vapid_keys();
        assert!(!public_key.is_empty());
        assert!(!private_key.is_empty());
        assert_ne!(public_key, private_key);
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let pub_bytes = engine.decode(&public_key).unwrap();
        assert_eq!(pub_bytes.len(), 65);
        assert_eq!(pub_bytes[0], 0x04);
        let priv_bytes = engine.decode(&private_key).unwrap();
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

    #[tokio::test]
    async fn test_send_push_stub() {
        let subscription = PushSubscription {
            endpoint: "https://example.com/push".to_string(),
            keys: PushSubscriptionKeys {
                p256dh: "test-p256dh".to_string(),
                auth: "test-auth".to_string(),
            },
        };
        let result = send_push(&subscription, "Test Title", "Test Body").await;
        assert!(result.is_ok());
    }
}
