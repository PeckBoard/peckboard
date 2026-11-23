//! Publisher authentication: Ed25519 keys, single-use nonces, and the
//! proof of work a never-seen key pays before its first publish.
//!
//! Signed messages (UTF-8):
//!
//! ```text
//! publish: "peckboard-skills-publish-v1\n" + nonce + "\n" + hex(sha256(skill_md))
//! yank:    "peckboard-skills-yank-v1\n"    + nonce + "\n" + name + "@" + version
//! other:   "peckboard-skills-request-v1\n" + METHOD + "\n" + PATH + "\n" + nonce
//!          + "\n" + hex(sha256(body))   (sent as X-Peck-* headers)
//! ```
//!
//! PoW: `sha256(nonce + pow)` (both as the UTF-8 strings sent) must have at
//! least `pow_difficulty` leading zero bits.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};

pub const PUBLISH_CONTEXT: &str = "peckboard-skills-publish-v1";
pub const YANK_CONTEXT: &str = "peckboard-skills-yank-v1";
pub const REQUEST_CONTEXT: &str = "peckboard-skills-request-v1";
/// Headers carrying a signed request.
pub const H_KEY: &str = "x-peck-key";
pub const H_NONCE: &str = "x-peck-nonce";
pub const H_POW: &str = "x-peck-pow";
pub const H_SIGNATURE: &str = "x-peck-signature";
/// How long an issued nonce stays valid.
pub const NONCE_TTL: Duration = Duration::from_secs(300);
/// Most outstanding nonces held at once (memory bound).
pub const MAX_NONCES: usize = 50_000;
/// Longest accepted PoW string.
pub const MAX_POW_LEN: usize = 128;

/// Base64 in any of the common alphabets, padded or not.
pub fn decode_b64(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    [&STANDARD, &STANDARD_NO_PAD, &URL_SAFE, &URL_SAFE_NO_PAD]
        .iter()
        .find_map(|e| e.decode(s).ok())
}

/// A publisher key: the verifying key plus its canonical text form
/// (standard padded base64), which is what the store keys on.
pub fn parse_public_key(s: &str) -> Option<(VerifyingKey, String)> {
    let bytes: [u8; 32] = decode_b64(s)?.try_into().ok()?;
    let key = VerifyingKey::from_bytes(&bytes).ok()?;
    if key.is_weak() {
        return None;
    }
    Some((key, STANDARD.encode(bytes)))
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub fn publish_message(nonce: &str, skill_md: &str) -> String {
    format!(
        "{PUBLISH_CONTEXT}\n{nonce}\n{}",
        sha256_hex(skill_md.as_bytes())
    )
}

pub fn yank_message(nonce: &str, name: &str, version: &str) -> String {
    format!("{YANK_CONTEXT}\n{nonce}\n{name}@{version}")
}

/// Signed-header requests: `"peckboard-skills-request-v1\n" + METHOD +
/// "\n" + PATH + "\n" + nonce + "\n" + hex(sha256(body))`, with `PATH`
/// the request path without its query string.
pub fn request_message(method: &str, path: &str, nonce: &str, body: &[u8]) -> String {
    format!(
        "{REQUEST_CONTEXT}\n{method}\n{path}\n{nonce}\n{}",
        sha256_hex(body)
    )
}

/// Strict Ed25519 verification of a base64 signature over `msg`.
pub fn verify(key: &VerifyingKey, msg: &str, signature_b64: &str) -> bool {
    let Some(bytes) = decode_b64(signature_b64) else {
        return false;
    };
    let Ok(sig) = Signature::from_slice(&bytes) else {
        return false;
    };
    key.verify_strict(msg.as_bytes(), &sig).is_ok()
}

pub fn leading_zero_bits(hash: &[u8]) -> u32 {
    let mut n = 0;
    for b in hash {
        if *b == 0 {
            n += 8;
        } else {
            return n + b.leading_zeros();
        }
    }
    n
}

pub fn pow_ok(nonce: &str, pow: &str, bits: u32) -> bool {
    if bits == 0 {
        return true;
    }
    if pow.len() > MAX_POW_LEN {
        return false;
    }
    let mut h = Sha256::new();
    h.update(nonce.as_bytes());
    h.update(pow.as_bytes());
    leading_zero_bits(&h.finalize()) >= bits
}

/// Brute-force a PoW string for `nonce` (what a client does; tests).
pub fn solve_pow(nonce: &str, bits: u32) -> String {
    (0u64..)
        .map(|i| i.to_string())
        .find(|p| pow_ok(nonce, p, bits))
        .expect("pow space exhausted")
}

#[derive(Debug, Clone)]
struct Issued {
    key: String,
    expires: Instant,
    pow_bits: u32,
}

/// Outstanding challenge nonces, in memory only (a restart simply
/// invalidates them; clients ask again).
#[derive(Default)]
pub struct Nonces {
    map: HashMap<String, Issued>,
}

impl Nonces {
    /// Issue a fresh 32-byte nonce (standard base64) bound to `key`, with
    /// the PoW difficulty fixed now. `None` when the table is full.
    pub fn issue(&mut self, key: &str, pow_bits: u32) -> Option<String> {
        let now = Instant::now();
        if self.map.len() >= MAX_NONCES {
            self.map.retain(|_, v| v.expires > now);
            if self.map.len() >= MAX_NONCES {
                return None;
            }
        }
        let nonce = STANDARD.encode(rand::random::<[u8; 32]>());
        self.map.insert(
            nonce.clone(),
            Issued {
                key: key.to_string(),
                expires: now + NONCE_TTL,
                pow_bits,
            },
        );
        Some(nonce)
    }

    /// Consume `nonce` (single use — removed whether or not it then checks
    /// out). Returns its PoW difficulty if it was issued to `key` and has
    /// not expired.
    pub fn take(&mut self, nonce: &str, key: &str) -> Option<u32> {
        let issued = self.map.remove(nonce)?;
        (issued.key == key && issued.expires > Instant::now()).then_some(issued.pow_bits)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pow_counts_leading_zero_bits() {
        assert_eq!(leading_zero_bits(&[0, 0x0f, 0xff]), 12);
        assert_eq!(leading_zero_bits(&[0x80]), 0);
        let p = solve_pow("abc", 8);
        assert!(pow_ok("abc", &p, 8));
        assert!(pow_ok("abc", "anything", 0));
    }

    #[test]
    fn nonces_are_single_use_and_key_bound() {
        let mut n = Nonces::default();
        let a = n.issue("k1", 3).unwrap();
        assert_eq!(n.take(&a, "k2"), None);
        // Consumed by the wrong-key attempt too.
        assert_eq!(n.take(&a, "k1"), None);
        let b = n.issue("k1", 3).unwrap();
        assert_eq!(n.take(&b, "k1"), Some(3));
        assert_eq!(n.take(&b, "k1"), None);
    }
}
