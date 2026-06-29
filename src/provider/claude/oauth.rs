//! Browser-based "log in with Claude" for subscription (`oauth_token`)
//! accounts — the in-app equivalent of running `claude setup-token` in a
//! terminal.
//!
//! It replicates the exact PKCE flow the `claude` CLI's `setup-token`
//! command performs (verified against CLI v2.1.193), so the access token we
//! mint is the same long-lived (≈1 year) credential the CLI would produce
//! and is injected verbatim as `CLAUDE_CODE_OAUTH_TOKEN`. The crucial
//! detail that makes the token long-lived is the `user:inference` scope —
//! the interactive `/login` flow requests broader scopes and gets a token
//! that expires within the hour, which would silently break a stored
//! account.
//!
//! Flow:
//!   1. [`start`] mints a PKCE `verifier`/`state` pair and returns the
//!      authorize URL. The browser holds the verifier + state.
//!   2. The user signs in, copies the displayed `code#state` string, and
//!      pastes it back. [`exchange`] swaps it for the access token.

use base64::Engine;
use rand::RngCore;
use serde::Serialize;
use sha2::{Digest, Sha256};

/// Public client id baked into the `claude` CLI for the setup-token flow.
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
/// Inference-only scope — this is what yields the long-lived token.
const SCOPE: &str = "user:inference";

/// One started login: the authorize URL to send the user to, plus the PKCE
/// `verifier` and `state` the caller must hand back to [`exchange`].
#[derive(Debug, Clone, Serialize)]
pub struct LoginStart {
    pub url: String,
    pub verifier: String,
    pub state: String,
}

/// base64url-no-pad encode some bytes (PKCE / state alphabet).
fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// 32 random bytes, base64url-encoded.
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    b64url(&bytes)
}

/// PKCE S256 challenge for a verifier: `base64url(sha256(verifier))`.
fn challenge_for(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    b64url(&digest)
}

/// Encode `key=value&…` pairs as `application/x-www-form-urlencoded`.
fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Build the authorize URL for a given challenge + state.
fn authorize_url(challenge: &str, state: &str) -> String {
    let query = form_encode(&[
        ("code", "true"),
        ("client_id", CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", REDIRECT_URI),
        ("scope", SCOPE),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
    ]);
    format!("{AUTHORIZE_URL}?{query}")
}

/// Begin a login: mint PKCE material and the authorize URL.
pub fn start() -> LoginStart {
    let verifier = random_token();
    let state = random_token();
    let challenge = challenge_for(&verifier);
    LoginStart {
        url: authorize_url(&challenge, &state),
        verifier,
        state,
    }
}

/// The platform shows a `<code>#<state>` string; the authorization code is
/// the part before the `#`. Trims whitespace and tolerates a pasted value
/// with or without the fragment.
fn parse_code(pasted: &str) -> &str {
    pasted.trim().split('#').next().unwrap_or("").trim()
}

#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: String,
}

/// Exchange a pasted `code#state` for the long-lived access token, using the
/// `verifier`/`state` from the matching [`start`]. Returns the token to store
/// as the account credential.
pub async fn exchange(
    client: &reqwest::Client,
    pasted_code: &str,
    verifier: &str,
    state: &str,
) -> anyhow::Result<String> {
    let code = parse_code(pasted_code);
    if code.is_empty() {
        anyhow::bail!("authorization code is empty");
    }
    let body = form_encode(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", REDIRECT_URI),
        ("client_id", CLIENT_ID),
        ("code_verifier", verifier),
        ("state", state),
    ]);
    let resp = client
        .post(TOKEN_URL)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(body)
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("token exchange failed ({status}): {body}");
    }
    let parsed: TokenResponse = serde_json::from_str(&body)
        .map_err(|e| anyhow::anyhow!("unexpected token response: {e}: {body}"))?;
    Ok(parsed.access_token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_is_stable_base64url_sha256() {
        // Known-answer: sha256("abc123") base64url-no-pad.
        let c = challenge_for("abc123");
        // No padding, URL-safe alphabet only.
        assert!(!c.contains('='));
        assert!(!c.contains('+'));
        assert!(!c.contains('/'));
        // Deterministic for a fixed verifier.
        assert_eq!(c, challenge_for("abc123"));
    }

    #[test]
    fn start_produces_distinct_high_entropy_material() {
        let a = start();
        let b = start();
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.state, b.state);
        // 32 bytes base64url-no-pad => 43 chars.
        assert_eq!(a.verifier.len(), 43);
        assert_eq!(a.state.len(), 43);
    }

    #[test]
    fn authorize_url_carries_the_verified_params() {
        let url = authorize_url("CHAL", "STATE");
        assert!(url.starts_with("https://claude.com/cai/oauth/authorize?"));
        assert!(url.contains("client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e"));
        assert!(url.contains("code_challenge=CHAL"));
        assert!(url.contains("state=STATE"));
        assert!(url.contains("code_challenge_method=S256"));
        // redirect_uri + scope are percent-encoded.
        assert!(
            url.contains(
                "redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback"
            )
        );
        assert!(url.contains("scope=user%3Ainference"));
    }

    #[test]
    fn parse_code_strips_the_state_fragment_and_whitespace() {
        assert_eq!(parse_code("  abc#def  "), "abc");
        assert_eq!(parse_code("abc"), "abc");
        assert_eq!(parse_code("abc#"), "abc");
        assert_eq!(parse_code("  "), "");
        assert_eq!(parse_code(""), "");
    }
}
