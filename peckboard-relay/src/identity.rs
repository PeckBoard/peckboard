//! Permanent box identity: one Ed25519 keypair per Peckboard box, separate
//! from the per-pairing keys derived from a pairing secret.
//!
//! The relay keeps a registry of identity public keys a human registered on
//! the relay's `/register` page (proof-of-work gated). With the relay's
//! registration gate on, only a box that proves possession of a registered
//! identity may use the relayed data path; direct connections, rendezvous
//! and STUN stay open to everyone.
//!
//! Proof of possession ([`identity_message`]) binds the relay's handshake
//! nonce and the TLS exporter of the signaling session — exactly like the
//! pairing membership proof — so a captured signature is useless on any
//! other connection.

use std::fmt;
use std::io::Write;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::RngCore;

use crate::keys::EXPORTER_LEN;

const IDENTITY_CONTEXT: &[u8] = b"peckrelay/3 box-identity";

/// A box's permanent identity key. Deliberately not `Display`; `Debug`
/// shows only the public key.
#[derive(Clone)]
pub struct BoxIdentity {
    signing: SigningKey,
}

impl fmt::Debug for BoxIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoxIdentity")
            .field("public_key", &encode_key(&self.public_key()))
            .finish_non_exhaustive()
    }
}

impl BoxIdentity {
    pub fn generate() -> Self {
        let mut seed = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut seed);
        Self::from_seed(seed)
    }

    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            signing: SigningKey::from_bytes(&seed),
        }
    }
    /// Load the identity stored at `path`, or create one there (file mode
    /// 0600, written atomically). The file holds the base64url seed.
    ///
    /// Creates silently — only for callers with no device pinning the key.
    /// Once a device has enrolled (pinned the public key), use
    /// [`load_or_create_with`](Self::load_or_create_with) with
    /// `create = false`: a new key would lock every such device out.
    pub fn load_or_create(path: &Path) -> std::io::Result<Self> {
        Self::load_or_create_with(path, true)
    }

    /// [`load_or_create`](Self::load_or_create) with an explicit creation
    /// policy: with `create = false` a missing file is
    /// `Err(ErrorKind::NotFound)` and nothing is written.
    pub fn load_or_create_with(path: &Path, create: bool) -> std::io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let seed = decode_key(s.trim()).ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("{}: not a box identity", path.display()),
                    )
                })?;
                Ok(Self::from_seed(seed))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
                let id = Self::generate();
                let seed = encode_key(&id.signing.to_bytes());
                write_private(path, format!("{seed}\n").as_bytes())?;
                Ok(id)
            }
            Err(e) => Err(e),
        }
    }

    /// The signing key, for the tunnel's TLS certificate and enrollment
    /// grants (prefixes disjoint from [`identity_message`]).
    #[cfg_attr(not(feature = "tunnel"), allow(dead_code))]
    pub(crate) fn signing_key(&self) -> &SigningKey {
        &self.signing
    }

    /// [`fingerprint`] of this identity.
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.public_key())
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    /// The public key as unpadded base64url — what the registration page,
    /// the `/api/registered` endpoint and the registry file use.
    pub fn public_key_b64(&self) -> String {
        encode_key(&self.public_key())
    }

    /// Proof of possession for one signaling session (see
    /// [`identity_message`]).
    pub fn sign_session(
        &self,
        nonce: &[u8; 32],
        rendezvous_id: &[u8; 32],
        exporter: &[u8; EXPORTER_LEN],
    ) -> [u8; 64] {
        let msg = identity_message(&self.public_key(), nonce, rendezvous_id, exporter);
        self.signing.sign(&msg).to_bytes()
    }
}

/// Bytes a box signs with its identity key to prove possession on one
/// session: the relay's challenge nonce and the TLS exporter make it
/// single-use; the rendezvous id ties it to this registration.
pub fn identity_message(
    identity_key: &[u8; 32],
    nonce: &[u8; 32],
    rendezvous_id: &[u8; 32],
    exporter: &[u8; EXPORTER_LEN],
) -> Vec<u8> {
    let mut m = Vec::with_capacity(IDENTITY_CONTEXT.len() + 32 * 3 + EXPORTER_LEN);
    m.extend_from_slice(IDENTITY_CONTEXT);
    m.extend_from_slice(identity_key);
    m.extend_from_slice(nonce);
    m.extend_from_slice(rendezvous_id);
    m.extend_from_slice(exporter);
    m
}

/// Relay-side check; false (never panics) on any malformed or weak key or
/// non-canonical signature.
pub fn verify_identity(
    identity_key: &[u8; 32],
    nonce: &[u8; 32],
    rendezvous_id: &[u8; 32],
    exporter: &[u8; EXPORTER_LEN],
    signature: &[u8; 64],
) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(identity_key) else {
        return false;
    };
    if vk.is_weak() {
        return false;
    }
    let sig = Signature::from_bytes(signature);
    vk.verify_strict(
        &identity_message(identity_key, nonce, rendezvous_id, exporter),
        &sig,
    )
    .is_ok()
}

/// 32 bytes as unpadded base64url (43 chars).
pub fn encode_key(key: &[u8; 32]) -> String {
    URL_SAFE_NO_PAD.encode(key)
}

/// Inverse of [`encode_key`]; tolerates trailing `=` padding.
pub fn decode_key(s: &str) -> Option<[u8; 32]> {
    URL_SAFE_NO_PAD
        .decode(s.trim().trim_end_matches('='))
        .ok()?
        .try_into()
        .ok()
}

const FINGERPRINT_CONTEXT: &[u8] = b"peckboard box-fp/1";

/// Human-comparable box fingerprint, 80 bits:
/// `base32(SHA-256("peckboard box-fp/1" ‖ key))[..16]` as
/// `XXXX-XXXX-XXXX-XXXX` (RFC 4648 alphabet, no padding). The box, the app
/// and the CLI all show this string.
pub fn fingerprint(key: &[u8; 32]) -> String {
    use sha2::{Digest, Sha256};
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let digest = Sha256::new()
        .chain_update(FINGERPRINT_CONTEXT)
        .chain_update(key)
        .finalize();
    // 16 chars × 5 bits = the first 10 bytes.
    let mut bits: u128 = 0;
    for b in &digest[..10] {
        bits = (bits << 8) | u128::from(*b);
    }
    let mut out = String::with_capacity(19);
    for i in 0..16 {
        if i > 0 && i % 4 == 0 {
            out.push('-');
        }
        let idx = (bits >> (75 - 5 * i)) & 0x1f;
        out.push(ALPHABET[idx as usize] as char);
    }
    out
}
/// A usable Ed25519 public key (on the curve, not small-order).
pub fn is_valid_public_key(key: &[u8; 32]) -> bool {
    VerifyingKey::from_bytes(key).is_ok_and(|vk| !vk.is_weak())
}

/// Write `data` to `path` atomically (temp file + rename), mode 0600.
pub(crate) fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(d) = dir {
        std::fs::create_dir_all(d)?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.tmp{}", rand::random::<u32>()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let res = (|| {
        let mut f = opts.open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("peckrelay-id-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn load_or_create_persists_with_private_perms() {
        let d = tmp_dir();
        let p = d.join("sub/box-identity");
        let a = BoxIdentity::load_or_create(&p).unwrap();
        let b = BoxIdentity::load_or_create(&p).unwrap();
        assert_eq!(a.public_key(), b.public_key());
        assert_eq!(decode_key(&a.public_key_b64()), Some(a.public_key()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::write(&p, "garbage").unwrap();
        assert!(BoxIdentity::load_or_create(&p).is_err());
        let _ = std::fs::remove_dir_all(d);
    }
    #[test]
    fn load_without_create_never_writes() {
        let d = tmp_dir();
        let p = d.join("box-identity");
        let e = BoxIdentity::load_or_create_with(&p, false).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
        assert!(!p.exists());
        let a = BoxIdentity::load_or_create_with(&p, true).unwrap();
        let b = BoxIdentity::load_or_create_with(&p, false).unwrap();
        assert_eq!(a.public_key(), b.public_key());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn fingerprint_fixed_vector() {
        let fp = fingerprint(&[7; 32]);
        // Cross-checked: base32(sha256(b"peckboard box-fp/1" + b"\x07" * 32))[:16].
        assert_eq!(fp, "HF27-SQLY-XYDE-WWUQ");
        assert_eq!(fp.len(), 19);
        assert_ne!(fp, fingerprint(&[8; 32]));
    }

    #[test]
    fn proof_is_bound_to_the_session() {
        let id = BoxIdentity::generate();
        let (n, r, ex) = ([1u8; 32], [2u8; 32], [3u8; EXPORTER_LEN]);
        let sig = id.sign_session(&n, &r, &ex);
        let pk = id.public_key();
        assert!(verify_identity(&pk, &n, &r, &ex, &sig));
        // Another session (exporter / nonce) or another key: rejected.
        assert!(!verify_identity(&pk, &n, &r, &[4; EXPORTER_LEN], &sig));
        assert!(!verify_identity(&pk, &[9; 32], &r, &ex, &sig));
        let other = BoxIdentity::generate().public_key();
        assert!(!verify_identity(&other, &n, &r, &ex, &sig));
        assert!(!verify_identity(&[0; 32], &n, &r, &ex, &sig));
    }
}
