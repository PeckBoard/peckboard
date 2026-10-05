//! Pairing-link intake: whatever the user scanned, pasted or deep-linked →
//! `PairingLink`, plus the slot a deep link waits in until the user confirms.

use peckboard_relay::tunnel::{LINK_PREFIX, PairingLink};

const HTTPS_BASE: &str = "https://peckboard.com/pair";

/// Accepts both link forms (`https://peckboard.com/pair#…` and
/// `peckboard://pair/…`) plus the usual paste noise: surrounding
/// whitespace, quotes or angle brackets, and text around it ("Pair your
/// phone: peckboard://pair/…"). Errors are user-facing.
pub fn parse_link(raw: &str) -> Result<PairingLink, String> {
    // `PairingLink::parse` finds the link inside text and ends it at the
    // first whitespace; turning the wrapping punctuation into whitespace
    // makes `"<link>"` end where the link does.
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if matches!(c, '"' | '\'' | '<' | '>' | '`') {
                ' '
            } else {
                c
            }
        })
        .collect();
    if !cleaned.contains(LINK_PREFIX) && !cleaned.contains(HTTPS_BASE) {
        return Err(format!(
            "That isn't a PeckBoard pairing link (it should start with {HTTPS_BASE} or {LINK_PREFIX})."
        ));
    }
    PairingLink::parse(&cleaned).map_err(|e| {
        let e = format!("{e:#}");
        if e.contains("newer PeckBoard app") {
            "This link needs a newer PeckBoard app. Update the app, then open the link again."
                .to_string()
        } else {
            format!(
                "This pairing link is damaged or incomplete — copy it again from your box ({e})."
            )
        }
    })
}

/// A deep-linked pairing link for the shell UI to confirm. Never paired
/// automatically: the user sees the relay host and the box fingerprint and
/// taps Pair, which pairs the link Rust holds for `id` — not a string the
/// page sends back.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairPrompt {
    pub id: String,
    /// The raw text, so an invalid link can be shown for correction.
    pub link: String,
    /// Relay host from the link; `None` when the link is invalid.
    pub relay: Option<String>,
    pub error: Option<String>,
    /// `XXXX-XXXX-XXXX-XXXX` of the box key a v2 link pins.
    pub box_fingerprint: Option<String>,
    /// Advisory expiry (unix seconds); the box enforces it.
    pub expires_at: Option<u64>,
    /// A v1 link: an older box, no fingerprint to compare.
    pub legacy: bool,
}

impl PairPrompt {
    pub fn from_link(id: &str, raw: &str) -> Self {
        let mut p = Self {
            id: id.to_string(),
            link: raw.trim().to_string(),
            relay: None,
            error: None,
            box_fingerprint: None,
            expires_at: None,
            legacy: false,
        };
        match parse_link(raw) {
            Ok(l) => {
                p.relay = Some(l.relay.clone());
                p.box_fingerprint = l.box_fingerprint();
                p.expires_at = l.expires;
                p.legacy = !l.is_v2();
            }
            Err(e) => p.error = Some(e),
        }
        p
    }
}

/// Where an opened pairing link waits. One at a time: a link that arrives
/// while another is on the confirm screen is dropped, so a page can't
/// swap the link under the user between "looks right" and "Pair".
#[derive(Debug, Default)]
pub enum PairSlot {
    #[default]
    Empty,
    /// Parked; nothing shown yet (a later link replaces it).
    Waiting(String),
    /// On the confirm screen (or being paired) as prompt `id`.
    Showing { id: String, raw: String },
}

impl PairSlot {
    /// A link was opened. `false`: dropped, another one is being confirmed.
    pub fn offer(&mut self, raw: &str) -> bool {
        match self {
            PairSlot::Showing { .. } => false,
            _ => {
                *self = PairSlot::Waiting(raw.to_string());
                true
            }
        }
    }

    /// Hand the waiting link to the shell UI (`Waiting` → `Showing`).
    pub fn take(&mut self) -> Option<PairPrompt> {
        // Only a waiting link moves; one that is showing stays reserved.
        let raw = match self {
            PairSlot::Waiting(raw) => std::mem::take(raw),
            _ => return None,
        };
        let id = new_prompt_id();
        let prompt = PairPrompt::from_link(&id, &raw);
        *self = PairSlot::Showing { id, raw };
        Some(prompt)
    }

    /// The raw link shown as prompt `id`, if that's what is showing. The
    /// slot stays `Showing` until [`clear`](Self::clear), so a second link
    /// opened while this one pairs is still dropped.
    pub fn confirm(&self, id: &str) -> Option<String> {
        match self {
            PairSlot::Showing { id: cur, raw } if cur == id => Some(raw.clone()),
            _ => None,
        }
    }

    /// Prompt `id` is done (paired, failed or dismissed).
    pub fn clear(&mut self, id: &str) -> bool {
        match self {
            PairSlot::Showing { id: cur, .. } if cur == id => {
                *self = PairSlot::Empty;
                true
            }
            _ => false,
        }
    }

    #[cfg(test)]
    pub fn is_showing(&self) -> bool {
        matches!(self, PairSlot::Showing { .. })
    }
}

fn new_prompt_id() -> String {
    use rand::RngCore;
    let mut b = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use peckboard_relay::identity::BoxIdentity;
    use peckboard_relay::keys::PairingSecret;
    use peckboard_relay::tunnel::HTTPS_LINK_PREFIX;

    fn sample(relay: &str) -> (PairingLink, String) {
        let link = PairingLink::new(PairingSecret::from_bytes([7; 32]), relay);
        let uri = link.to_uri();
        (link, uri)
    }

    fn sample_v2(relay: &str) -> PairingLink {
        PairingLink::new_v2(
            PairingSecret::from_bytes([8; 32]),
            relay,
            BoxIdentity::from_seed([4; 32]).public_key(),
            1_800_000_000,
        )
    }

    #[test]
    fn parses_bare_and_noisy_links() {
        let (link, uri) = sample("relay.example.com:8443");
        for raw in [
            uri.clone(),
            format!("  {uri}\n"),
            format!("\"{uri}\""),
            format!("<{uri}>"),
            format!("Pair your phone: {uri} (one device only)"),
        ] {
            let got = parse_link(&raw).unwrap_or_else(|e| panic!("{raw:?}: {e}"));
            assert_eq!(got.to_uri(), link.to_uri());
            assert_eq!(got.relay, "relay.example.com:8443");
            assert!(!got.is_v2());
        }
    }

    #[test]
    fn parses_v2_https_and_custom_scheme_forms() {
        let link = sample_v2("relay.example.com");
        let https = link.to_https();
        let uri = link.to_uri();
        assert!(https.starts_with(HTTPS_LINK_PREFIX), "{https}");
        for raw in [
            https.clone(),
            uri.clone(),
            format!("Scan or open: <{https}> — works once"),
            format!("'{uri}'"),
        ] {
            let got = parse_link(&raw).unwrap_or_else(|e| panic!("{raw:?}: {e}"));
            assert!(got.is_v2());
            assert_eq!(got.to_uri(), uri);
            assert_eq!(got.box_key, link.box_key);
            assert_eq!(got.expires, Some(1_800_000_000));
            assert_eq!(got.relay, "relay.example.com");
        }
        // Default relay when the https form omits `r`.
        let default = sample_v2("relay.peckboard.com");
        assert!(!default.to_https().contains("&r="));
        assert_eq!(
            parse_link(&default.to_https()).unwrap().relay,
            "relay.peckboard.com"
        );
    }

    #[test]
    fn default_relay_when_absent() {
        let (_, uri) = sample("relay.peckboard.com");
        let bare = uri.split('?').next().unwrap();
        assert_eq!(parse_link(bare).unwrap().relay, "relay.peckboard.com");
    }

    #[test]
    fn rejects_garbage_with_readable_errors() {
        let not_a_link = parse_link("https://example.com").unwrap_err();
        assert!(not_a_link.contains("isn't a PeckBoard pairing link"));
        assert!(parse_link("peckboard://pair/not-base64!").is_err());
        assert!(parse_link("peckboard://pair/").is_err());
        assert!(parse_link("https://peckboard.com/pair#v=2&s=abc").is_err());
        // A future link version.
        let newer = parse_link("https://peckboard.com/pair#v=3&s=abc").unwrap_err();
        assert!(newer.contains("newer PeckBoard app"), "{newer}");
    }

    #[test]
    fn deep_link_prompt_shows_relay_fingerprint_or_error() {
        let (_, uri) = sample("relay.example.com:8443");
        let p = PairPrompt::from_link("p1", &uri);
        assert_eq!(
            (p.relay.as_deref(), p.error.clone(), p.link.clone()),
            (Some("relay.example.com:8443"), None, uri)
        );
        assert!(p.legacy && p.box_fingerprint.is_none() && p.expires_at.is_none());

        let v2 = sample_v2("relay.example.com");
        let p = PairPrompt::from_link("p2", &v2.to_https());
        assert_eq!(p.id, "p2");
        assert!(!p.legacy);
        assert_eq!(p.box_fingerprint, v2.box_fingerprint());
        assert_eq!(p.expires_at, Some(1_800_000_000));
        let fp = p.box_fingerprint.unwrap();
        assert_eq!(fp.len(), 19, "{fp}");
        assert_eq!(fp.matches('-').count(), 3);

        let bad = PairPrompt::from_link("p3", "peckboard://pair/xyz");
        assert!(bad.relay.is_none() && bad.error.is_some());
    }

    /// SECURITY (link swap): while a link is on the confirm screen — or
    /// being paired — a second one is dropped, so what the user checked is
    /// what gets paired. Before it is shown, a later link replaces it.
    #[test]
    fn slot_rejects_a_second_link_while_one_is_showing() {
        let (_, a) = sample("a.example.com");
        let (_, b) = sample("b.example.com");
        let mut slot = PairSlot::default();
        assert!(slot.take().is_none());
        assert!(slot.offer(&a));
        assert!(slot.offer(&b), "nothing shown yet: replaced");
        let p = slot.take().unwrap();
        assert_eq!(p.relay.as_deref(), Some("b.example.com"));
        assert!(slot.is_showing());
        assert!(slot.take().is_none(), "taken once");

        assert!(!slot.offer(&a), "showing: dropped");
        assert_eq!(slot.confirm(&p.id).as_deref(), Some(b.as_str()));
        assert!(slot.confirm("other").is_none());
        // Still showing while the pairing runs.
        assert!(!slot.offer(&a));
        assert!(!slot.clear("other"));
        assert!(slot.clear(&p.id));
        assert!(slot.confirm(&p.id).is_none());
        assert!(slot.offer(&a), "cleared: accepts again");
        assert_ne!(slot.take().unwrap().id, p.id, "fresh id per prompt");
    }
}
