//! Pairing secrets for remote-access devices: generation, sealing at rest,
//! and the pairing link.
//!
//! The 32-byte link secret `S` is everything a device needs to reach this
//! box through the relay (and, for a v2 link, to enroll its own key once),
//! so it is sealed with AES-256-GCM under the server-held
//! `remote_access_key` file (`<data_dir>/remote_access_key`, `0600`,
//! outside every agent's sandbox) with the device id as AAD — a ciphertext
//! copied onto another row won't open. Pairing v2 seals two more 32-byte
//! secrets per device under the same key with distinct AADs
//! (`<id>/rendezvous/2` for `R`, `<id>/link-refuse` for the retired `S`),
//! so a blob can't be moved between columns either. [`DeviceSecret`] has
//! no `Serialize` and a redacting `Debug`, so the plaintext can't leak into
//! a response or a log line by accident; the pairing link built from it is
//! returned exactly once, by the create route.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use peckboard_relay::keys::{PairingSecret, RendezvousSecret};
use peckboard_relay::tunnel::PairingLink;
use rand::RngCore;
use rand::rngs::OsRng;

use crate::db::models::RemoteDevice;

pub const SECRET_LEN: usize = 32;
/// Data-dir file holding the AES-256-GCM key that seals pairing secrets.
pub const KEY_FILE: &str = "remote_access_key";
/// AAD suffix of the sealed rendezvous secret `R`.
const AAD_RENDEZVOUS: &str = "/rendezvous/2";
/// AAD suffix of the link secret kept for the refuse loop after activation.
const AAD_LINK_REFUSE: &str = "/link-refuse";

/// A device's plaintext link secret `S`. Only ever held in memory.
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

    /// `S` as the relay crate's type: the rendezvous and tunnel secret of a
    /// legacy pairing or an unused link.
    pub fn pairing_secret(&self) -> PairingSecret {
        PairingSecret::from_bytes(self.0)
    }

    /// `peckboard://pair/<base64url(S)>?relay=<host>` — a legacy (v1) link.
    /// Built by the relay crate so the box and the client can never
    /// disagree on the format.
    pub fn pairing_link(&self, relay_host: &str) -> String {
        PairingLink::new(self.pairing_secret(), relay_host).to_uri()
    }

    /// A v2 link: pins the box identity key and carries the expiry (unix
    /// seconds; advisory for the device, enforced by the box).
    pub fn link_v2(&self, relay_host: &str, box_key: [u8; 32], expires: u64) -> PairingLink {
        PairingLink::new_v2(self.pairing_secret(), relay_host, box_key, expires)
    }
}

fn aad(device_id: &str, suffix: &str) -> Vec<u8> {
    let mut a = device_id.as_bytes().to_vec();
    a.extend_from_slice(suffix.as_bytes());
    a
}

/// `(ciphertext, nonce)` of a 32-byte secret sealed under `key`, bound to
/// `aad`.
fn seal_bytes(
    key: &[u8],
    aad: &[u8],
    secret: &[u8; SECRET_LEN],
) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let cipher =
        Aes256Gcm::new_from_slice(key).map_err(|_| anyhow::anyhow!("invalid vault key length"))?;
    let n = Nonce::try_from(nonce.as_slice()).map_err(|_| anyhow::anyhow!("bad nonce"))?;
    let ct = cipher
        .encrypt(&n, Payload { msg: secret, aad })
        .map_err(|_| anyhow::anyhow!("encryption failed"))?;
    Ok((ct, nonce.to_vec()))
}

/// Open a sealed 32-byte secret. Fails on a tag mismatch (wrong key, blob
/// tampered, or moved to another row / column).
fn open_bytes(key: &[u8], aad: &[u8], ct: &[u8], nonce: &[u8]) -> anyhow::Result<[u8; SECRET_LEN]> {
    let cipher =
        Aes256Gcm::new_from_slice(key).map_err(|_| anyhow::anyhow!("invalid vault key length"))?;
    let n = Nonce::try_from(nonce).map_err(|_| anyhow::anyhow!("invalid nonce length"))?;
    let pt = cipher
        .decrypt(&n, Payload { msg: ct, aad })
        .map_err(|_| anyhow::anyhow!("pairing secret could not be decrypted"))?;
    pt.try_into()
        .map_err(|_| anyhow::anyhow!("pairing secret has the wrong length"))
}

/// `(ciphertext, nonce)` of `secret` sealed under `key`, bound to `device_id`.
pub fn seal(
    key: &[u8],
    device_id: &str,
    secret: &DeviceSecret,
) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    seal_bytes(key, device_id.as_bytes(), secret.as_bytes())
}

/// Open a device row's sealed link secret.
pub fn open(key: &[u8], row: &RemoteDevice) -> anyhow::Result<DeviceSecret> {
    open_bytes(
        key,
        row.id.as_bytes(),
        &row.secret_ciphertext,
        &row.secret_nonce,
    )
    .map(DeviceSecret)
}

/// Seal a device's rendezvous secret `R` (`remote_device_enrollments`).
pub fn seal_rendezvous(
    key: &[u8],
    device_id: &str,
    r: &RendezvousSecret,
) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    seal_bytes(key, &aad(device_id, AAD_RENDEZVOUS), r.as_bytes())
}

pub fn open_rendezvous(
    key: &[u8],
    device_id: &str,
    ct: &[u8],
    nonce: &[u8],
) -> anyhow::Result<RendezvousSecret> {
    open_bytes(key, &aad(device_id, AAD_RENDEZVOUS), ct, nonce).map(RendezvousSecret::from_bytes)
}

/// Seal the retired link secret `S` for the refuse loop (activation).
pub fn seal_link_refuse(
    key: &[u8],
    device_id: &str,
    s: &DeviceSecret,
) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    seal_bytes(key, &aad(device_id, AAD_LINK_REFUSE), s.as_bytes())
}

pub fn open_link_refuse(
    key: &[u8],
    device_id: &str,
    ct: &[u8],
    nonce: &[u8],
) -> anyhow::Result<DeviceSecret> {
    open_bytes(key, &aad(device_id, AAD_LINK_REFUSE), ct, nonce).map(DeviceSecret)
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

    /// The three per-device blobs use distinct AADs: a blob can't be read
    /// back as another column's secret.
    #[test]
    fn column_aads_are_distinct() {
        let key = [7u8; 32];
        let r = RendezvousSecret::from_bytes([3u8; 32]);
        let (ct, nonce) = seal_rendezvous(&key, "d1", &r).unwrap();
        assert!(open_rendezvous(&key, "d1", &ct, &nonce).unwrap() == r);
        assert!(open_rendezvous(&key, "d2", &ct, &nonce).is_err());
        assert!(open_link_refuse(&key, "d1", &ct, &nonce).is_err());
        assert!(open(&key, &row("d1", ct, nonce)).is_err());
        let s = DeviceSecret::generate();
        let (ct, nonce) = seal_link_refuse(&key, "d1", &s).unwrap();
        assert_eq!(
            open_link_refuse(&key, "d1", &ct, &nonce)
                .unwrap()
                .as_bytes(),
            s.as_bytes()
        );
        assert!(open(&key, &row("d1", ct, nonce)).is_err());
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
        let v2 = s.link_v2("relay.peckboard.com", [9u8; 32], 1_800_000_000);
        assert!(v2.is_v2());
        assert!(
            v2.to_https()
                .starts_with("https://peckboard.com/pair#v=2&s=")
        );
    }

    impl DeviceSecret {
        fn from_test(b: [u8; SECRET_LEN]) -> Self {
            Self(b)
        }
    }
}
