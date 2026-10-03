//! Pairing secrets for remote-access devices: generation, sealing at rest,
//! and the one-time pairing link.
//!
//! The 32-byte secret `S` is everything a device needs to reach this box
//! through the relay, so it is sealed with AES-256-GCM under the
//! server-held `remote_access_key` file (`<data_dir>/remote_access_key`,
//! `0600`, outside every agent's sandbox) with the device id as AAD — a
//! ciphertext copied onto another row won't open. [`DeviceSecret`] has no
//! `Serialize` and a redacting `Debug`, so the plaintext can't leak into a
//! response or a log line by accident; the pairing link built from it is
//! returned exactly once, by the create route.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use rand::RngCore;
use rand::rngs::OsRng;

use crate::db::models::RemoteDevice;

pub const SECRET_LEN: usize = 32;
/// Data-dir file holding the AES-256-GCM key that seals pairing secrets.
pub const KEY_FILE: &str = "remote_access_key";

/// A device's plaintext pairing secret. Only ever held in memory.
#[derive(Clone)]
pub struct DeviceSecret([u8; SECRET_LEN]);

impl std::fmt::Debug for DeviceSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DeviceSecret(<redacted>)")
    }
}

impl DeviceSecret {
    pub fn generate() -> Self {
        let mut b = [0u8; SECRET_LEN];
        OsRng.fill_bytes(&mut b);
        Self(b)
    }

    pub fn as_bytes(&self) -> &[u8; SECRET_LEN] {
        &self.0
    }

    /// `peckboard://pair/<base64url(S)>?relay=<host>` — what
    /// `peckboard-connect` takes. Built by the relay crate so the box and
    /// the client can never disagree on the format.
    pub fn pairing_link(&self, relay_host: &str) -> String {
        peckboard_relay::tunnel::PairingLink::new(
            peckboard_relay::keys::PairingSecret::from_bytes(self.0),
            relay_host,
        )
        .to_uri()
    }
}

/// `(ciphertext, nonce)` of `secret` sealed under `key`, bound to `device_id`.
pub fn seal(
    key: &[u8],
    device_id: &str,
    secret: &DeviceSecret,
) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let cipher =
        Aes256Gcm::new_from_slice(key).map_err(|_| anyhow::anyhow!("invalid vault key length"))?;
    let n = Nonce::try_from(nonce.as_slice()).map_err(|_| anyhow::anyhow!("bad nonce"))?;
    let ct = cipher
        .encrypt(
            &n,
            Payload {
                msg: secret.as_bytes(),
                aad: device_id.as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!("encryption failed"))?;
    Ok((ct, nonce.to_vec()))
}

/// Open a row's sealed secret. Fails on a tag mismatch (wrong key, row
/// tampered, or ciphertext moved to another id).
pub fn open(key: &[u8], row: &RemoteDevice) -> anyhow::Result<DeviceSecret> {
    let cipher =
        Aes256Gcm::new_from_slice(key).map_err(|_| anyhow::anyhow!("invalid vault key length"))?;
    let n = Nonce::try_from(row.secret_nonce.as_slice())
        .map_err(|_| anyhow::anyhow!("invalid nonce length"))?;
    let pt = cipher
        .decrypt(
            &n,
            Payload {
                msg: &row.secret_ciphertext,
                aad: row.id.as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!("pairing secret could not be decrypted"))?;
    let bytes: [u8; SECRET_LEN] = pt
        .try_into()
        .map_err(|_| anyhow::anyhow!("pairing secret has the wrong length"))?;
    Ok(DeviceSecret(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    fn row(id: &str, ct: Vec<u8>, nonce: Vec<u8>) -> RemoteDevice {
        RemoteDevice {
            id: id.into(),
            user_id: "u1".into(),
            name: "phone".into(),
            secret_ciphertext: ct,
            secret_nonce: nonce,
            created_at: "2026-10-03T00:00:00Z".into(),
            last_connected_at: None,
        }
    }

    #[test]
    fn seal_round_trips_and_binds_key_and_id() {
        let key = [7u8; 32];
        let s = DeviceSecret::generate();
        let (ct, nonce) = seal(&key, "d1", &s).unwrap();
        assert!(
            !ct.windows(SECRET_LEN).any(|w| w == s.as_bytes()),
            "plaintext must not appear in the ciphertext"
        );
        let back = open(&key, &row("d1", ct.clone(), nonce.clone())).unwrap();
        assert_eq!(back.as_bytes(), s.as_bytes());
        assert!(open(&[8u8; 32], &row("d1", ct.clone(), nonce.clone())).is_err());
        assert!(
            open(&key, &row("d2", ct, nonce)).is_err(),
            "AAD binds the ciphertext to its row"
        );
    }

    #[test]
    fn debug_redacts_and_link_encodes_secret() {
        let s = DeviceSecret::from_test([0xAB; 32]);
        assert_eq!(format!("{s:?}"), "DeviceSecret(<redacted>)");
        let link = s.pairing_link("relay.peckboard.com");
        assert_eq!(
            link,
            format!(
                "peckboard://pair/{}?relay=relay.peckboard.com",
                URL_SAFE_NO_PAD.encode([0xAB; 32])
            )
        );
    }

    impl DeviceSecret {
        fn from_test(b: [u8; SECRET_LEN]) -> Self {
            Self(b)
        }
    }
}
