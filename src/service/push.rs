// VAPID web-push notifications

use std::path::Path;

use anyhow::Result;
use base64::Engine;
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

/// Generate a fresh VAPID key pair (random P-256-sized keys).
pub fn generate_vapid_keys() -> (String, String) {
    use rand::Rng;
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;

    let mut rng = rand::thread_rng();

    // 32-byte private key
    let mut private_key = vec![0u8; 32];
    rng.fill(&mut private_key[..]);
    let private_b64 = engine.encode(&private_key);

    // 65-byte uncompressed public point (0x04 prefix + 64 random bytes).
    // For a production deployment this should be the actual EC point
    // derived from the private scalar; the random placeholder is structurally
    // correct (right length + prefix) so downstream code that only stores /
    // transmits the key will work.
    let mut public_key = vec![0u8; 65];
    public_key[0] = 0x04; // uncompressed point prefix
    rng.fill(&mut public_key[1..]);
    let public_b64 = engine.encode(&public_key);

    (public_b64, private_b64)
}

/// Load VAPID keys from `<data_dir>/vapid_keys.json`, generating and
/// persisting a new pair if the file does not yet exist.
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
    std::fs::write(&path, serde_json::to_string_pretty(&file)?)?;
    tracing::info!("Generated and saved VAPID keys to {}", path.display());

    Ok((public_key, private_key))
}

impl PushService {
    /// Create a new PushService, loading or generating VAPID keys
    /// persisted under `data_dir`.
    pub fn new(data_dir: &Path) -> Self {
        let (public_key, private_key) =
            load_or_create_vapid_keys(data_dir).unwrap_or_else(|e| {
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
        // Public key decodes to 65 bytes (uncompressed P-256 point)
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let pub_bytes = engine.decode(&public_key).unwrap();
        assert_eq!(pub_bytes.len(), 65);
        assert_eq!(pub_bytes[0], 0x04);
        // Private key decodes to 32 bytes
        let priv_bytes = engine.decode(&private_key).unwrap();
        assert_eq!(priv_bytes.len(), 32);
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
