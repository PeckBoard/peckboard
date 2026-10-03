//! Everything derived from the 32-byte pairing secret `S`.
//!
//! `S` is shared out-of-band by the box (QR code / link) and never reaches
//! the relay. HKDF-SHA256 expands it into three independent values:
//!
//! - **rendezvous id** — the unguessable name both peers register under;
//! - **Ed25519 keypair** — proves membership: the relay records the public
//!   key on first registration and every later peer must sign the relay's
//!   challenge with the matching private key;
//! - **E2E key** — XChaCha20-Poly1305 key sealing every peer↔peer message
//!   the relay forwards, so the relay only ever handles opaque blobs.

use std::sync::atomic::{AtomicU64, Ordering};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::Sha256;

use crate::proto::Role;

pub const SECRET_LEN: usize = 32;
pub const NONCE_LEN: usize = 24;
pub const TAG_LEN: usize = 16;
/// Smallest possible sealed blob (empty plaintext).
pub const SEALED_OVERHEAD: usize = NONCE_LEN + TAG_LEN;

const HKDF_SALT: &[u8] = b"peckboard-relay/v1";
const AUTH_CONTEXT: &[u8] = b"peckrelay/1 auth";
const E2E_CONTEXT: &[u8] = b"peckrelay/1 e2e";
/// TLS exporter label binding the membership signature to this TLS session,
/// so a captured signature can't be replayed on another connection.
pub const EXPORTER_LABEL: &[u8] = b"EXPORTER-peckrelay-auth";
pub const EXPORTER_LEN: usize = 32;

/// The out-of-band pairing secret. Deliberately not `Debug`/`Display`.
#[derive(Clone)]
pub struct PairingSecret([u8; SECRET_LEN]);

impl PairingSecret {
    pub fn generate() -> Self {
        let mut s = [0u8; SECRET_LEN];
        rand::rngs::OsRng.fill_bytes(&mut s);
        Self(s)
    }

    pub fn from_bytes(bytes: [u8; SECRET_LEN]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; SECRET_LEN] {
        &self.0
    }

    pub fn derive(&self) -> DerivedKeys {
        let hk = Hkdf::<Sha256>::new(Some(HKDF_SALT), &self.0);
        let mut rendezvous_id = [0u8; 32];
        let mut seed = [0u8; 32];
        let mut e2e = [0u8; 32];
        // 32-byte outputs are always within HKDF-SHA256's limit.
        hk.expand(b"rendezvous-id", &mut rendezvous_id)
            .expect("hkdf");
        hk.expand(b"ed25519-seed", &mut seed).expect("hkdf");
        hk.expand(b"e2e-xchacha20poly1305", &mut e2e).expect("hkdf");
        DerivedKeys {
            rendezvous_id,
            signing: SigningKey::from_bytes(&seed),
            e2e: E2eKey {
                key: e2e,
                rendezvous_id,
            },
        }
    }
}

pub struct DerivedKeys {
    pub rendezvous_id: [u8; 32],
    pub signing: SigningKey,
    pub e2e: E2eKey,
}

impl DerivedKeys {
    pub fn public_key(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    pub fn sign_challenge(
        &self,
        nonce: &[u8; 32],
        role: Role,
        exporter: &[u8; EXPORTER_LEN],
    ) -> [u8; 64] {
        let msg = auth_message(nonce, &self.rendezvous_id, role, exporter);
        self.signing.sign(&msg).to_bytes()
    }
}

/// Bytes the peer signs to answer a challenge.
pub fn auth_message(
    nonce: &[u8; 32],
    rendezvous_id: &[u8; 32],
    role: Role,
    exporter: &[u8; EXPORTER_LEN],
) -> Vec<u8> {
    let mut m = Vec::with_capacity(AUTH_CONTEXT.len() + 32 + 32 + 1 + EXPORTER_LEN);
    m.extend_from_slice(AUTH_CONTEXT);
    m.extend_from_slice(nonce);
    m.extend_from_slice(rendezvous_id);
    m.push(role as u8);
    m.extend_from_slice(exporter);
    m
}

/// Relay-side check. Returns false (never panics) on any malformed key or
/// signature, so callers can fold it into a single constant-shape decision.
pub fn verify_auth(
    public_key: &[u8; 32],
    nonce: &[u8; 32],
    rendezvous_id: &[u8; 32],
    role: Role,
    exporter: &[u8; EXPORTER_LEN],
    signature: &[u8; 64],
) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(public_key) else {
        return false;
    };
    let sig = Signature::from_bytes(signature);
    vk.verify(&auth_message(nonce, rendezvous_id, role, exporter), &sig)
        .is_ok()
}

/// XChaCha20-Poly1305 box for peer↔peer messages. The AAD binds the
/// rendezvous id and the *sender's* role, so a blob can't be reflected back
/// at its sender or replayed into another pairing.
#[derive(Clone)]
pub struct E2eKey {
    key: [u8; 32],
    rendezvous_id: [u8; 32],
}

impl E2eKey {
    fn aad(&self, sender: Role, context: &[u8]) -> Vec<u8> {
        let mut a = Vec::with_capacity(E2E_CONTEXT.len() + 33 + context.len());
        a.extend_from_slice(E2E_CONTEXT);
        a.extend_from_slice(&self.rendezvous_id);
        a.push(sender as u8);
        a.extend_from_slice(context);
        a
    }

    /// `nonce || ciphertext || tag`.
    pub fn seal(&self, sender: Role, context: &[u8], plaintext: &[u8]) -> Vec<u8> {
        let cipher = XChaCha20Poly1305::new((&self.key).into());
        let mut nonce = [0u8; NONCE_LEN];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let aad = self.aad(sender, context);
        let ct = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .expect("xchacha encrypt");
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        out
    }

    pub fn open(&self, sender: Role, context: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
        if sealed.len() < SEALED_OVERHEAD {
            return None;
        }
        let cipher = XChaCha20Poly1305::new((&self.key).into());
        let (nonce, ct) = sealed.split_at(NONCE_LEN);
        let aad = self.aad(sender, context);
        cipher
            .decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad: &aad })
            .ok()
    }
}

/// Max clock skew (either direction) accepted on a counted message.
pub const REPLAY_WINDOW_MS: u64 = 120_000;
/// `counter (u64 BE) || timestamp ms (u64 BE)`, inside the ciphertext.
pub const COUNTED_HEADER_LEN: usize = 16;

impl E2eKey {
    /// Like [`seal`](Self::seal), but prefixes the plaintext with a
    /// per-sender `counter` and the sender's unix-ms clock, so both are
    /// authenticated (and hidden from the relay).
    pub fn seal_counted(
        &self,
        sender: Role,
        context: &[u8],
        counter: u64,
        ts_ms: u64,
        plaintext: &[u8],
    ) -> Vec<u8> {
        let mut pt = Vec::with_capacity(COUNTED_HEADER_LEN + plaintext.len());
        pt.extend_from_slice(&counter.to_be_bytes());
        pt.extend_from_slice(&ts_ms.to_be_bytes());
        pt.extend_from_slice(plaintext);
        self.seal(sender, context, &pt)
    }

    /// Opens a [`seal_counted`](Self::seal_counted) blob and enforces replay
    /// protection for `sender` (whose state is `guard`): the counter must
    /// exceed the last accepted one and the timestamp must be within
    /// ±[`REPLAY_WINDOW_MS`] of `now_ms`. Rejects leave `guard` unchanged.
    pub fn open_counted(
        &self,
        sender: Role,
        context: &[u8],
        sealed: &[u8],
        guard: &mut ReplayGuard,
        now_ms: u64,
    ) -> Option<Vec<u8>> {
        let mut pt = self.open(sender, context, sealed)?;
        if pt.len() < COUNTED_HEADER_LEN {
            return None;
        }
        let counter = u64::from_be_bytes(pt[..8].try_into().ok()?);
        let ts_ms = u64::from_be_bytes(pt[8..16].try_into().ok()?);
        if !guard.accept(counter, ts_ms, now_ms) {
            return None;
        }
        pt.drain(..COUNTED_HEADER_LEN);
        Some(pt)
    }
}

/// Receiver-side replay state for ONE sender (a role within one rendezvous
/// id — the AAD already pins both, so one guard per peer suffices).
#[derive(Debug, Default)]
pub struct ReplayGuard {
    last: Option<u64>,
}

impl ReplayGuard {
    pub fn accept(&mut self, counter: u64, ts_ms: u64, now_ms: u64) -> bool {
        if ts_ms.abs_diff(now_ms) > REPLAY_WINDOW_MS {
            return false;
        }
        if self.last.is_some_and(|l| counter <= l) {
            return false;
        }
        self.last = Some(counter);
        true
    }
}

/// Sender-side counter. Seeded from the clock (callers pass unix µs), so a
/// sender that restarts or reconnects resumes above every counter it used
/// before without persisting state; strictly increasing even if the clock
/// stalls or steps back.
#[derive(Debug, Default)]
pub struct MsgCounter(AtomicU64);

impl MsgCounter {
    pub fn next(&self, now_us: u64) -> u64 {
        let prev = self
            .0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |p| {
                Some(p.saturating_add(1).max(now_us))
            })
            .expect("closure always returns Some");
        prev.saturating_add(1).max(now_us)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derivation_is_deterministic_and_separated() {
        let s = PairingSecret::from_bytes([7; 32]);
        let a = s.derive();
        let b = s.derive();
        assert_eq!(a.rendezvous_id, b.rendezvous_id);
        assert_eq!(a.public_key(), b.public_key());
        assert_ne!(a.rendezvous_id, a.public_key());
        let other = PairingSecret::from_bytes([8; 32]).derive();
        assert_ne!(a.rendezvous_id, other.rendezvous_id);
    }

    #[test]
    fn seal_binds_sender_role() {
        let k = PairingSecret::generate().derive().e2e;
        let sealed = k.seal(Role::Box, b"msg", b"hello");
        assert_eq!(k.open(Role::Box, b"msg", &sealed).unwrap(), b"hello");
        assert!(k.open(Role::Device, b"msg", &sealed).is_none());
        assert!(k.open(Role::Box, b"cand", &sealed).is_none());
    }

    fn pair() -> (E2eKey, ReplayGuard) {
        (
            PairingSecret::generate().derive().e2e,
            ReplayGuard::default(),
        )
    }

    const NOW: u64 = 1_700_000_000_000;

    #[test]
    fn in_order_messages_accepted() {
        let (k, mut g) = pair();
        for (i, body) in [b"a", b"b", b"c"].iter().enumerate() {
            let s = k.seal_counted(Role::Box, b"msg", 10 + i as u64, NOW, *body);
            assert_eq!(
                k.open_counted(Role::Box, b"msg", &s, &mut g, NOW).unwrap(),
                *body
            );
        }
    }

    #[test]
    fn replayed_message_rejected() {
        let (k, mut g) = pair();
        let s = k.seal_counted(Role::Box, b"msg", 1, NOW, b"hi");
        assert!(k.open_counted(Role::Box, b"msg", &s, &mut g, NOW).is_some());
        assert!(k.open_counted(Role::Box, b"msg", &s, &mut g, NOW).is_none());
    }

    #[test]
    fn reordered_older_rejected() {
        let (k, mut g) = pair();
        let older = k.seal_counted(Role::Box, b"msg", 5, NOW, b"old");
        let newer = k.seal_counted(Role::Box, b"msg", 6, NOW, b"new");
        assert!(
            k.open_counted(Role::Box, b"msg", &newer, &mut g, NOW)
                .is_some()
        );
        assert!(
            k.open_counted(Role::Box, b"msg", &older, &mut g, NOW)
                .is_none()
        );
    }

    #[test]
    fn stale_or_future_timestamp_rejected() {
        let (k, mut g) = pair();
        let skew = REPLAY_WINDOW_MS + 1;
        let stale = k.seal_counted(Role::Box, b"msg", 1, NOW - skew, b"x");
        let future = k.seal_counted(Role::Box, b"msg", 2, NOW + skew, b"x");
        assert!(
            k.open_counted(Role::Box, b"msg", &stale, &mut g, NOW)
                .is_none()
        );
        assert!(
            k.open_counted(Role::Box, b"msg", &future, &mut g, NOW)
                .is_none()
        );
        // Rejections don't advance the window: an in-window message still lands.
        let ok = k.seal_counted(Role::Box, b"msg", 1, NOW - REPLAY_WINDOW_MS, b"x");
        assert!(
            k.open_counted(Role::Box, b"msg", &ok, &mut g, NOW)
                .is_some()
        );
    }

    #[test]
    fn counter_survives_restart_and_clock_stall() {
        let a = MsgCounter::default();
        assert_eq!(a.next(100), 100);
        assert_eq!(a.next(100), 101);
        assert_eq!(a.next(50), 102);
        // A fresh sender (reconnect) seeds from the clock, past the old run.
        assert_eq!(MsgCounter::default().next(200), 200);
    }
}
