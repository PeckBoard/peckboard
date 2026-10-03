//! Pairing-link intake: whatever the user scanned or pasted → `PairingLink`.

use peckboard_relay::tunnel::PairingLink;

const PREFIX: &str = "peckboard://pair/";

/// Accepts the bare link plus the usual paste noise: surrounding
/// whitespace, quotes or angle brackets, and text around it ("Pair your
/// phone: peckboard://pair/…"). Errors are user-facing.
pub fn parse_link(raw: &str) -> Result<PairingLink, String> {
    let start = raw.find(PREFIX).ok_or_else(|| {
        "That isn't a PeckBoard pairing link (it should start with peckboard://pair/).".to_string()
    })?;
    let candidate = raw[start..]
        .split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`'))
        .next()
        .unwrap_or_default();
    PairingLink::parse(candidate).map_err(|e| {
        format!("This pairing link is damaged or incomplete — copy it again from your box ({e}).")
    })
}

/// A deep-linked pairing link for the shell UI to confirm. Never paired
/// automatically: the user sees the relay host and taps Pair.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairPrompt {
    pub link: String,
    /// Relay host from the link; `None` when the link is invalid.
    pub relay: Option<String>,
    pub error: Option<String>,
}

impl PairPrompt {
    pub fn from_link(raw: &str) -> Self {
        match parse_link(raw) {
            Ok(l) => Self {
                link: raw.trim().to_string(),
                relay: Some(l.relay),
                error: None,
            },
            Err(e) => Self {
                link: raw.trim().to_string(),
                relay: None,
                error: Some(e),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use peckboard_relay::keys::PairingSecret;

    fn sample(relay: &str) -> (PairingLink, String) {
        let link = PairingLink::new(PairingSecret::from_bytes([7; 32]), relay);
        let uri = link.to_uri();
        (link, uri)
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
        }
    }

    #[test]
    fn default_relay_when_absent() {
        let (_, uri) = sample("relay.peckboard.com");
        let bare = uri.split('?').next().unwrap();
        assert_eq!(parse_link(bare).unwrap().relay, "relay.peckboard.com");
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_link("https://example.com").is_err());
        assert!(parse_link("peckboard://pair/not-base64!").is_err());
        assert!(parse_link("peckboard://pair/").is_err());
    }

    #[test]
    fn deep_link_prompt_shows_relay_or_error() {
        let (_, uri) = sample("relay.example.com:8443");
        let p = PairPrompt::from_link(&uri);
        assert_eq!(
            (p.relay.as_deref(), p.error, p.link),
            (Some("relay.example.com:8443"), None, uri)
        );
        let bad = PairPrompt::from_link("peckboard://pair/xyz");
        assert!(bad.relay.is_none() && bad.error.is_some());
    }
}
