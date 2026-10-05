//! Pairing links and the credentials each side runs a tunnel loop with.
//!
//! A v1 link carries only the pairing secret `S`, a forever bearer
//! credential. A v2 link (pairing v2) also pins the box identity key `B`
//! and an expiry, and is good for one enrollment: the device proves `S`
//! once, enrolls its own key `D`, and gets a rendezvous secret `R` back
//! ([`EnrolledCredential`]). From then on the tunnel is authenticated by
//! `B` and `D`, and `R` only names the rendezvous.

use std::collections::HashSet;
use std::fmt;

use anyhow::{anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::SigningKey;
use rand::RngCore;

use crate::identity::{BoxIdentity, fingerprint, is_valid_public_key};
use crate::keys::{PairingSecret, RendezvousSecret, SECRET_LEN};

use super::DEFAULT_RELAY;

/// `peckboard://pair/<S>?relay=…` (v1; v2 adds `&v=2&k=…&e=…`).
pub const LINK_PREFIX: &str = "peckboard://pair/";
/// `https://peckboard.com/pair#v=2&s=…&k=…&e=…[&r=…]`: every field in the
/// fragment, which never reaches a server or a `Referer`.
pub const HTTPS_LINK_PREFIX: &str = "https://peckboard.com/pair#";
const HTTPS_LINK_BASE: &str = "https://peckboard.com/pair";
/// Newest link version this build understands.
pub const LINK_VERSION: u8 = 2;

// ---- pairing link -------------------------------------------------------

/// A pairing link. `version` 1: `S` + relay only (legacy). `version` 2:
/// also the box identity key and an expiry (unix seconds; advisory — the
/// box enforces it, a device never refuses on its own clock).
#[derive(Clone)]
pub struct PairingLink {
    pub secret: PairingSecret,
    pub relay: String,
    pub version: u8,
    /// Box identity public key; `Some` iff `version == 2`.
    pub box_key: Option<[u8; 32]>,
    /// Expiry, unix seconds; `Some` iff `version == 2`.
    pub expires: Option<u64>,
}

impl fmt::Debug for PairingLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingLink")
            .field("relay", &self.relay)
            .field("version", &self.version)
            .field("box_fingerprint", &self.box_fingerprint())
            .field("expires", &self.expires)
            .finish_non_exhaustive()
    }
}

impl PairingLink {
    /// A v1 (legacy) link.
    pub fn new(secret: PairingSecret, relay: &str) -> Self {
        Self {
            secret,
            relay: relay.to_string(),
            version: 1,
            box_key: None,
            expires: None,
        }
    }

    /// A v2 link pinning `box_key`, expiring at `expires` (unix seconds).
    pub fn new_v2(secret: PairingSecret, relay: &str, box_key: [u8; 32], expires: u64) -> Self {
        Self {
            secret,
            relay: relay.to_string(),
            version: 2,
            box_key: Some(box_key),
            expires: Some(expires),
        }
    }

    pub fn is_v2(&self) -> bool {
        self.version >= 2
    }

    /// Fingerprint of the pinned box key (v2 links).
    pub fn box_fingerprint(&self) -> Option<String> {
        self.box_key.as_ref().map(fingerprint)
    }

    /// `peckboard://pair/<S>?relay=<host>`, plus `&v=2&k=<B>&e=<secs>` for
    /// v2. A v1-only parser still reads `S` and the relay from it.
    pub fn to_uri(&self) -> String {
        let mut s = format!(
            "{LINK_PREFIX}{}?relay={}",
            URL_SAFE_NO_PAD.encode(self.secret.as_bytes()),
            self.relay
        );
        if let (true, Some(k), Some(e)) = (self.is_v2(), self.box_key, self.expires) {
            s.push_str(&format!("&v=2&k={}&e={e}", URL_SAFE_NO_PAD.encode(k)));
        }
        s
    }

    /// `https://peckboard.com/pair#v=2&s=<S>&k=<B>&e=<secs>[&r=<relay>]`
    /// (`r` only when not the default relay). What the box shows.
    pub fn to_https(&self) -> String {
        let mut s = String::from(HTTPS_LINK_PREFIX);
        if let (true, Some(k), Some(e)) = (self.is_v2(), self.box_key, self.expires) {
            s.push_str(&format!(
                "v=2&s={}&k={}&e={e}",
                URL_SAFE_NO_PAD.encode(self.secret.as_bytes()),
                URL_SAFE_NO_PAD.encode(k)
            ));
        } else {
            s.push_str(&format!(
                "s={}",
                URL_SAFE_NO_PAD.encode(self.secret.as_bytes())
            ));
        }
        if self.relay != DEFAULT_RELAY {
            s.push_str(&format!("&r={}", self.relay));
        }
        s
    }

    /// Parse either form. Surrounding text is ignored (the link is found
    /// inside it and ends at the first whitespace); duplicate keys are
    /// rejected, unknown keys ignored.
    pub fn parse(link: &str) -> anyhow::Result<Self> {
        let text = link.trim();
        let https = find_https(text);
        let custom = text.find(LINK_PREFIX);
        let fields: Vec<(&str, &str)> = match (https, custom) {
            (Some((i, frag)), c) if c.is_none_or(|c| i < c) => {
                split_pairs(until_space(frag)).collect()
            }
            (_, Some(c)) => {
                let rest = until_space(&text[c + LINK_PREFIX.len()..]);
                let rest = rest.split('#').next().unwrap_or("");
                let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
                std::iter::once(("s", path))
                    .chain(split_pairs(query))
                    .collect()
            }
            _ => bail!("not a PeckBoard pairing link"),
        };
        let mut seen = HashSet::new();
        let mut get = std::collections::HashMap::new();
        for (k, v) in fields {
            let k = if k == "relay" { "r" } else { k };
            if !seen.insert(k) {
                bail!("pairing link: duplicate `{k}`");
            }
            get.insert(k, v);
        }
        let version = match get.get("v") {
            None => 1,
            Some(v) => v
                .parse::<u8>()
                .ok()
                .filter(|v| *v >= 1)
                .ok_or_else(|| anyhow!("pairing link: bad version"))?,
        };
        if version > LINK_VERSION {
            bail!("This link needs a newer PeckBoard app");
        }
        let s = get
            .get("s")
            .ok_or_else(|| anyhow!("pairing link: no secret"))?;
        let bytes = URL_SAFE_NO_PAD
            .decode(s.trim_end_matches('/').trim_end_matches('='))
            .map_err(|_| anyhow!("pairing link: secret is not base64url"))?;
        let secret: [u8; SECRET_LEN] = bytes
            .try_into()
            .map_err(|_| anyhow!("pairing link: secret must be {SECRET_LEN} bytes"))?;
        let relay = match get.get("r") {
            Some(r) if !r.is_empty() => {
                validate_relay_host(r)?;
                r.to_string()
            }
            _ => DEFAULT_RELAY.to_string(),
        };
        let mut out = Self::new(PairingSecret::from_bytes(secret), &relay);
        if version == 2 {
            let k = get
                .get("k")
                .and_then(|k| crate::identity::decode_key(k))
                .ok_or_else(|| anyhow!("pairing link: missing or malformed box key"))?;
            if !is_valid_public_key(&k) {
                bail!("pairing link: invalid box key");
            }
            let e = get
                .get("e")
                .and_then(|e| e.parse::<u64>().ok())
                .ok_or_else(|| anyhow!("pairing link: missing or malformed expiry"))?;
            out = Self::new_v2(out.secret, &relay, k, e);
        }
        Ok(out)
    }
}

/// Fragment after `https://peckboard.com/pair#` (or `/pair/#`).
fn find_https(text: &str) -> Option<(usize, &str)> {
    let i = text.find(HTTPS_LINK_BASE)?;
    let rest = &text[i + HTTPS_LINK_BASE.len()..];
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    rest.strip_prefix('#').map(|f| (i, f))
}

fn until_space(s: &str) -> &str {
    s.split(char::is_whitespace).next().unwrap_or("")
}

fn split_pairs(q: &str) -> impl Iterator<Item = (&str, &str)> {
    q.split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| kv.split_once('=').unwrap_or((kv, "")))
}

/// `host` or `host:port`, DNS-name / IP-literal characters only (same rule
/// as the box's settings).
pub fn validate_relay_host(h: &str) -> anyhow::Result<()> {
    let (host, port) = match h.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (h, None),
    };
    if host.is_empty()
        || h.len() > 253
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        bail!("pairing link: relay must be a hostname, optionally with :port");
    }
    if let Some(p) = port
        && p.parse::<u16>().map_or(true, |p| p == 0)
    {
        bail!("pairing link: relay port must be 1..=65535");
    }
    Ok(())
}

// ---- enrolled credential ------------------------------------------------

const CRED_VERSION: u8 = 1;

/// What an enrolled device keeps: `R`, its own key `D`, the pinned box key
/// `B` and the relay. One opaque string ([`encode`](Self::encode)), so one
/// keychain item or file holds it all. Neither `Debug` nor anything else
/// prints the secrets.
#[derive(Clone)]
pub struct EnrolledCredential {
    r: RendezvousSecret,
    device_key: SigningKey,
    box_key: [u8; 32],
    relay: String,
}

impl fmt::Debug for EnrolledCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EnrolledCredential")
            .field("relay", &self.relay)
            .field("box_fingerprint", &self.box_fingerprint())
            .finish_non_exhaustive()
    }
}

impl EnrolledCredential {
    pub const PREFIX: &'static str = "peckboard-cred:2:";

    pub fn new(
        r: RendezvousSecret,
        device_key: SigningKey,
        box_key: [u8; 32],
        relay: &str,
    ) -> Self {
        Self {
            r,
            device_key,
            box_key,
            relay: relay.to_string(),
        }
    }

    pub fn rendezvous(&self) -> &RendezvousSecret {
        &self.r
    }

    pub fn device_key(&self) -> &SigningKey {
        &self.device_key
    }

    pub fn device_public_key(&self) -> [u8; 32] {
        self.device_key.verifying_key().to_bytes()
    }

    pub fn box_key(&self) -> [u8; 32] {
        self.box_key
    }

    pub fn box_fingerprint(&self) -> String {
        fingerprint(&self.box_key)
    }

    pub fn relay(&self) -> &str {
        &self.relay
    }

    /// `peckboard-cred:2:` + b64u(ver ‖ R ‖ d_seed ‖ B ‖ relay_len ‖ relay).
    pub fn encode(&self) -> String {
        let relay = self.relay.as_bytes();
        let mut b = Vec::with_capacity(1 + 96 + 1 + relay.len());
        b.push(CRED_VERSION);
        b.extend_from_slice(self.r.as_bytes());
        b.extend_from_slice(&self.device_key.to_bytes());
        b.extend_from_slice(&self.box_key);
        b.push(relay.len() as u8);
        b.extend_from_slice(relay);
        format!("{}{}", Self::PREFIX, URL_SAFE_NO_PAD.encode(b))
    }

    pub fn parse(s: &str) -> anyhow::Result<Self> {
        let body = s
            .trim()
            .strip_prefix(Self::PREFIX)
            .ok_or_else(|| anyhow!("not a PeckBoard device credential"))?;
        let b = URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| anyhow!("device credential: not base64url"))?;
        if b.len() < 98 || b[0] != CRED_VERSION {
            bail!("device credential: unsupported or truncated");
        }
        let take = |i: usize| -> [u8; 32] { b[i..i + 32].try_into().expect("32 bytes") };
        let (r, d, k) = (take(1), take(33), take(65));
        let len = b[97] as usize;
        if b.len() != 98 + len {
            bail!("device credential: bad length");
        }
        let relay = std::str::from_utf8(&b[98..])
            .map_err(|_| anyhow!("device credential: relay is not UTF-8"))?;
        validate_relay_host(relay)?;
        if !is_valid_public_key(&k) {
            bail!("device credential: invalid box key");
        }
        Ok(Self::new(
            RendezvousSecret::from_bytes(r),
            SigningKey::from_bytes(&d),
            k,
            relay,
        ))
    }
}

// ---- refusals and modes -------------------------------------------------

/// Why the box refused an enrollment (`EnrollRefused`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RefuseReason {
    /// Another device already enrolled with this link.
    AlreadyUsed = 1,
    /// The link expired.
    Expired = 2,
    /// Revoked, legacy upgrades disabled, or a mode mismatch.
    NotEnrollable = 3,
    Internal = 4,
    RateLimited = 5,
}

impl RefuseReason {
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Unknown codes (a newer box) read as [`Internal`](Self::Internal).
    pub fn from_u8(b: u8) -> Self {
        match b {
            1 => Self::AlreadyUsed,
            2 => Self::Expired,
            3 => Self::NotEnrollable,
            5 => Self::RateLimited,
            _ => Self::Internal,
        }
    }

    /// `already_used` / `expired` / `not_enrollable` / `internal` /
    /// `rate_limited`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AlreadyUsed => "already_used",
            Self::Expired => "expired",
            Self::NotEnrollable => "not_enrollable",
            Self::Internal => "internal",
            Self::RateLimited => "rate_limited",
        }
    }

    /// What to tell the user.
    pub fn message(self) -> &'static str {
        match self {
            Self::AlreadyUsed => {
                "This pairing link was already used by another device. If that wasn't you, \
                 revoke it on your box and create a new one."
            }
            Self::Expired => "This pairing link expired. Create a new one on your box.",
            Self::NotEnrollable => {
                "Your box can't pair this device with this link. Create a new link on your box."
            }
            Self::Internal => "Your box couldn't complete pairing. Try again.",
            Self::RateLimited => "Too many pairing attempts. Wait a minute and try again.",
        }
    }

    /// A link refused for this reason will never enroll; retrying is
    /// pointless.
    pub fn is_final(self) -> bool {
        matches!(
            self,
            Self::AlreadyUsed | Self::Expired | Self::NotEnrollable
        )
    }

    /// QUIC close reason for an old (`/1`) client on a refusing link loop.
    pub(super) fn close_reason(self) -> &'static str {
        match self {
            Self::Expired => "link-expired",
            Self::NotEnrollable => "not-enrollable",
            _ => "link-used",
        }
    }
}

impl fmt::Display for RefuseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

/// What an `EnrollRequest` asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EnrollMode {
    /// First use of a v2 link (over ALPN `/2`).
    Link = 1,
    /// An existing v1 pairing moving to its own key (over ALPN `/1`).
    LegacyUpgrade = 2,
}

impl EnrollMode {
    pub(super) fn from_u8(b: u8) -> Option<Self> {
        match b {
            1 => Some(Self::Link),
            2 => Some(Self::LegacyUpgrade),
            _ => None,
        }
    }
}

/// What a box loop for a v2 link does with enrollment requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkMode {
    /// Pending (or staged: the handler re-delivers `R` to the same key).
    Enroll,
    /// Used or expired: every request is refused with this reason.
    Refuse(RefuseReason),
}

// ---- credentials --------------------------------------------------------

/// What one box loop serves (see the session policy in [`super`]).
#[derive(Clone)]
pub enum BoxCredential {
    /// A pairing from before v2: ALPN `/1` with `S`-derived certs, full
    /// service. With `identity`, the device may upgrade (enroll its own key
    /// over this connection, `0x03` mode 2) when a handler is installed.
    Legacy {
        s: PairingSecret,
        identity: Option<BoxIdentity>,
    },
    /// The `rid(S)` loop for a v2 link: ALPN `/2` (box cert `B`) is
    /// enrollment only; `/1` (an old app) is closed with `update-app` or the
    /// refusal reason.
    Link {
        s: PairingSecret,
        identity: BoxIdentity,
        mode: LinkMode,
    },
    /// The `rid(R)` loop of an enrolled device: ALPN `/2`, box cert `B`,
    /// client cert must be `device_key`.
    Enrolled {
        r: RendezvousSecret,
        identity: BoxIdentity,
        device_key: [u8; 32],
    },
}

impl fmt::Debug for BoxCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Legacy { identity, .. } => f
                .debug_struct("Legacy")
                .field("upgrade", &identity.is_some())
                .finish_non_exhaustive(),
            Self::Link { mode, .. } => f
                .debug_struct("Link")
                .field("mode", mode)
                .finish_non_exhaustive(),
            Self::Enrolled { device_key, .. } => f
                .debug_struct("Enrolled")
                .field("device", &fingerprint(device_key))
                .finish_non_exhaustive(),
        }
    }
}

impl BoxCredential {
    /// The secret to register at the relay with (`S`, or `S_R` for an
    /// enrolled device) — pass it to [`establish_with`](super::establish_with).
    pub fn relay_secret(&self) -> PairingSecret {
        match self {
            Self::Legacy { s, .. } | Self::Link { s, .. } => s.clone(),
            Self::Enrolled { r, .. } => r.relay_secret(),
        }
    }

    /// `legacy` / `link` / `enrolled`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Legacy { .. } => "legacy",
            Self::Link { .. } => "link",
            Self::Enrolled { .. } => "enrolled",
        }
    }

    pub(super) fn identity(&self) -> Option<&BoxIdentity> {
        match self {
            Self::Legacy { identity, .. } => identity.as_ref(),
            Self::Link { identity, .. } | Self::Enrolled { identity, .. } => Some(identity),
        }
    }
}

/// What a device connects with.
#[derive(Clone)]
pub enum DeviceCredential {
    /// A v1 link: ALPN `/1`, `S`-derived certs. With `upgrade_key` (the
    /// device's stored key `D`), [`run_device`](super::run_device) tries
    /// once to enroll it over the legacy connection.
    Legacy {
        link: PairingLink,
        upgrade_key: Option<SigningKey>,
    },
    /// A v2 link, not yet enrolled: ALPN `/2` pinned to `link.box_key`,
    /// enrolls `device_key` (store it before connecting, so a retry after a
    /// crash re-uses it).
    Link {
        link: PairingLink,
        device_key: SigningKey,
    },
    /// Enrolled: ALPN `/2` pinned to `B`, client cert `D`, rendezvous `R`.
    Enrolled(EnrolledCredential),
}

impl fmt::Debug for DeviceCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Legacy { link, upgrade_key } => f
                .debug_struct("Legacy")
                .field("link", link)
                .field("upgrade", &upgrade_key.is_some())
                .finish(),
            Self::Link { link, .. } => f
                .debug_struct("Link")
                .field("link", link)
                .finish_non_exhaustive(),
            Self::Enrolled(c) => f.debug_tuple("Enrolled").field(c).finish(),
        }
    }
}

impl DeviceCredential {
    /// A v2 link enrolls `device_key`; a v1 link connects as legacy and
    /// upgrades to `device_key`.
    pub fn from_link(link: PairingLink, device_key: SigningKey) -> Self {
        if link.is_v2() {
            Self::Link { link, device_key }
        } else {
            Self::Legacy {
                link,
                upgrade_key: Some(device_key),
            }
        }
    }

    pub fn relay_host(&self) -> &str {
        match self {
            Self::Legacy { link, .. } | Self::Link { link, .. } => &link.relay,
            Self::Enrolled(c) => c.relay(),
        }
    }

    /// `S`, or `S_R` once enrolled.
    pub fn relay_secret(&self) -> PairingSecret {
        match self {
            Self::Legacy { link, .. } | Self::Link { link, .. } => link.secret.clone(),
            Self::Enrolled(c) => c.rendezvous().relay_secret(),
        }
    }

    /// `legacy` / `link` / `enrolled`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Legacy { .. } => "legacy",
            Self::Link { .. } => "link",
            Self::Enrolled(_) => "enrolled",
        }
    }
}

/// v1 → `Legacy` (no upgrade); v2 → `Link` with a fresh random device key
/// that is NOT persisted anywhere — fine for one-shot use and tests, but a
/// caller that may retry after a crash should store a key and use
/// [`DeviceCredential::from_link`].
impl From<PairingLink> for DeviceCredential {
    fn from(link: PairingLink) -> Self {
        if link.is_v2() {
            let mut seed = [0u8; 32];
            rand::rngs::OsRng.fill_bytes(&mut seed);
            Self::Link {
                link,
                device_key: SigningKey::from_bytes(&seed),
            }
        } else {
            Self::Legacy {
                link,
                upgrade_key: None,
            }
        }
    }
}

impl From<EnrolledCredential> for DeviceCredential {
    fn from(c: EnrolledCredential) -> Self {
        Self::Enrolled(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(b: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(b)
    }

    fn box_key() -> [u8; 32] {
        BoxIdentity::from_seed([4; 32]).public_key()
    }

    #[test]
    fn link_v1_roundtrip() {
        let s = PairingSecret::generate();
        let l = PairingLink::new(s.clone(), "relay.example:4443");
        let uri = l.to_uri();
        assert!(uri.starts_with("peckboard://pair/"));
        let p = PairingLink::parse(&uri).unwrap();
        assert_eq!(p.secret.as_bytes(), s.as_bytes());
        assert_eq!(p.relay, "relay.example:4443");
        assert_eq!(p.version, 1);
        let bare = PairingLink::parse(uri.split('?').next().unwrap()).unwrap();
        assert_eq!(bare.relay, DEFAULT_RELAY);
        assert!(PairingLink::parse("peckboard://pair/AAAA").is_err());
        assert!(PairingLink::parse("https://x/pair/AAAA").is_err());
    }

    #[test]
    fn link_parse_matrix() {
        let s = PairingSecret::from_bytes([9; 32]);
        let l = PairingLink::new_v2(s.clone(), "relay.example:4443", box_key(), 1_800_000_000);
        // Both forms, with noise around them.
        for form in [l.to_uri(), l.to_https()] {
            let p = PairingLink::parse(&format!("  scan: {form} (from box)\n")).unwrap();
            assert_eq!(p.version, 2);
            assert_eq!(p.secret.as_bytes(), s.as_bytes());
            assert_eq!(p.box_key, Some(box_key()));
            assert_eq!(p.expires, Some(1_800_000_000));
            assert_eq!(p.relay, "relay.example:4443");
        }
        // Default relay is left out of the https form and restored.
        let d = PairingLink::new_v2(s.clone(), DEFAULT_RELAY, box_key(), 5);
        assert!(!d.to_https().contains("&r="));
        assert_eq!(
            PairingLink::parse(&d.to_https()).unwrap().relay,
            DEFAULT_RELAY
        );
        // Trailing-slash variant.
        let slash = d.to_https().replace("/pair#", "/pair/#");
        assert!(PairingLink::parse(&slash).is_ok());

        let (sb, kb) = (b64(&[9; 32]), b64(&box_key()));
        let h = |frag: &str| PairingLink::parse(&format!("{HTTPS_LINK_PREFIX}{frag}"));
        // v1 https; v>2; v2 without k / e; weak k; duplicates; unknown keys.
        assert_eq!(h(&format!("s={sb}")).unwrap().version, 1);
        let newer = h(&format!("v=3&s={sb}")).unwrap_err().to_string();
        assert!(newer.contains("newer PeckBoard app"), "{newer}");
        assert!(h(&format!("v=2&s={sb}&e=5")).is_err());
        assert!(h(&format!("v=2&s={sb}&k={kb}")).is_err());
        assert!(h(&format!("v=2&s={sb}&k={}&e=5", b64(&[0; 32]))).is_err());
        let mut identity_point = [0u8; 32];
        identity_point[0] = 1;
        assert!(h(&format!("v=2&s={sb}&k={}&e=5", b64(&identity_point))).is_err());
        assert!(h(&format!("v=2&s={sb}&s={sb}&k={kb}&e=5")).is_err());
        assert!(h(&format!("v=2&s={sb}&k={kb}&e=5&r=a.b&r=c.d")).is_err());
        assert!(h(&format!("v=2&s={sb}&k={kb}&e=5&zz=1")).is_ok());
        assert!(h(&format!("v=2&s={sb}&k={kb}&e=5&r=bad/host")).is_err());

        // A v1-only reader of the v2 custom-scheme form still finds S.
        let uri = l.to_uri();
        let rest = uri.strip_prefix(LINK_PREFIX).unwrap();
        let old_s = rest.split('?').next().unwrap();
        assert_eq!(URL_SAFE_NO_PAD.decode(old_s).unwrap(), s.as_bytes());
    }

    #[test]
    fn link_debug_hides_secret() {
        let s = PairingSecret::from_bytes([9; 32]);
        let l = PairingLink::new_v2(s, "r.example", box_key(), 5);
        let dbg = format!("{l:?}");
        assert!(!dbg.contains(&b64(&[9; 32])), "{dbg}");
    }

    #[test]
    fn enrolled_credential_roundtrip_and_redaction() {
        let c = EnrolledCredential::new(
            RendezvousSecret::from_bytes([1; 32]),
            SigningKey::from_bytes(&[2; 32]),
            box_key(),
            "relay.example:4443",
        );
        let s = c.encode();
        assert!(s.starts_with(EnrolledCredential::PREFIX));
        let p = EnrolledCredential::parse(&s).unwrap();
        assert!(p.rendezvous() == c.rendezvous());
        assert_eq!(p.device_key().to_bytes(), [2; 32]);
        assert_eq!(p.box_key(), box_key());
        assert_eq!(p.relay(), "relay.example:4443");
        let dbg = format!("{c:?} {:?}", DeviceCredential::from(c.clone()));
        for secret in [b64(&[1; 32]), b64(&[2; 32]), s.clone()] {
            assert!(!dbg.contains(&secret), "{dbg}");
        }
        assert!(EnrolledCredential::parse(&format!("{s}AA")).is_err());
        assert!(EnrolledCredential::parse("peckboard-cred:2:AAAA").is_err());
    }
}
