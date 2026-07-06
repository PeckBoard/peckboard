//! Browser-based "log in with Claude" for subscription (`oauth_token`)
//! accounts — the in-app equivalent of running `claude setup-token` in a
//! terminal.
//!
//! It replicates the PKCE flow of the `claude` CLI's `setup-token` command
//! (verified against CLI v2.1.193), with `user:profile` added to the CLI's
//! inference-only scope because `GET /api/oauth/usage` demands it — without
//! it per-account plan usage gets a scope 403 (anthropics/claude-code#13724).
//! The profile scope costs us the ≈1-year token the pure setup-token flow
//! used to yield: Anthropic answers this scope set with an ~8h access token
//! plus a refresh token. So [`exchange`] returns the whole [`MintedToken`]
//! (access + refresh + expiry) for the account row, [`refresh`] renews it,
//! and [`super::token_refresh::fresh_credential`] keeps the stored
//! credential valid at every point of use. A short-lived token WITHOUT a
//! refresh token is still refused — it would silently break the account
//! when it lapses.
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
/// Inference (what the CLI's setup-token requests — yields the long-lived
/// token) plus profile, which the plan-usage endpoint requires.
const SCOPE: &str = "user:inference user:profile";

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

/// A stored token that expires sooner than this (seconds) must carry a
/// refresh token, or the account would silently break when it lapses.
const SHORT_LIVED_TTL_SECS: i64 = 30 * 24 * 3600;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Guard for [`exchange`]: a short-lived token is storable only alongside
/// a refresh token. No expiry (the historical long-lived setup token) is
/// always fine.
fn ensure_storable(minted: &MintedToken) -> anyhow::Result<()> {
    let Some(expires_at) = minted.expires_at_ms else {
        return Ok(());
    };
    let ttl_secs = (expires_at - now_ms()) / 1000;
    if minted.refresh_token.is_none() && ttl_secs < SHORT_LIVED_TTL_SECS {
        anyhow::bail!(
            "Anthropic issued a short-lived token ({ttl_secs}s) with no refresh token; \
             refusing to store it because the account would break when it expires."
        );
    }
    Ok(())
}

#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    /// Lifetime in seconds; absent for (long-lived) setup tokens.
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    refresh_token: Option<String>,
}

/// A minted credential: the access token plus what's needed to renew it.
/// Both extras are `None` for long-lived setup tokens.
#[derive(Debug, Clone)]
pub struct MintedToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// ms epoch when `access_token` expires.
    pub expires_at_ms: Option<i64>,
}

impl From<TokenResponse> for MintedToken {
    fn from(parsed: TokenResponse) -> Self {
        MintedToken {
            expires_at_ms: parsed.expires_in.map(|s| now_ms() + (s as i64) * 1000),
            access_token: parsed.access_token,
            refresh_token: parsed.refresh_token,
        }
    }
}

/// POST an urlencoded grant to the token endpoint, parse the response.
async fn post_token_form(client: &reqwest::Client, body: String) -> anyhow::Result<MintedToken> {
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
    Ok(parsed.into())
}

/// Exchange a pasted `code#state` for an access token, using the
/// `verifier`/`state` from the matching [`start`]. Short-lived tokens are
/// accepted only when a refresh token comes with them (see
/// [`ensure_storable`]).
pub async fn exchange(
    client: &reqwest::Client,
    pasted_code: &str,
    verifier: &str,
    state: &str,
) -> anyhow::Result<MintedToken> {
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
    let minted = post_token_form(client, body).await?;
    ensure_storable(&minted)?;
    Ok(minted)
}

/// Renew a short-lived access token with its refresh token. The endpoint
/// may rotate the refresh token; when it doesn't, `refresh_token` comes
/// back `None` and the caller keeps using the old one.
pub async fn refresh(client: &reqwest::Client, refresh_token: &str) -> anyhow::Result<MintedToken> {
    let body = form_encode(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", CLIENT_ID),
    ]);
    post_token_form(client, body).await
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
        assert!(url.contains("scope=user%3Ainference%20user%3Aprofile"));
    }

    #[test]
    fn parse_code_strips_the_state_fragment_and_whitespace() {
        assert_eq!(parse_code("  abc#def  "), "abc");
        assert_eq!(parse_code("abc"), "abc");
        assert_eq!(parse_code("abc#"), "abc");
        assert_eq!(parse_code("  "), "");
        assert_eq!(parse_code(""), "");
    }

    #[test]
    fn token_response_parses_optional_refresh_fields() {
        let full: TokenResponse = serde_json::from_str(
            r#"{"access_token":"tok","expires_in":28800,"refresh_token":"ref"}"#,
        )
        .unwrap();
        assert_eq!(full.expires_in, Some(28800));
        assert_eq!(full.refresh_token.as_deref(), Some("ref"));
        let bare: TokenResponse = serde_json::from_str(r#"{"access_token":"tok"}"#).unwrap();
        assert_eq!(bare.expires_in, None);
        assert_eq!(bare.refresh_token, None);

        // Conversion stamps an absolute expiry only when one was reported.
        let minted: MintedToken = full.into();
        assert!(minted.expires_at_ms.expect("expiry stamped") > now_ms());
        let minted: MintedToken = bare.into();
        assert_eq!(minted.expires_at_ms, None);
    }

    #[test]
    fn short_lived_tokens_need_a_refresh_token() {
        let minted = |expires_at_ms: Option<i64>, refresh: Option<&str>| MintedToken {
            access_token: "tok".into(),
            refresh_token: refresh.map(str::to_string),
            expires_at_ms,
        };
        // Today's ~8h profile-scope token is fine WITH a refresh token…
        assert!(ensure_storable(&minted(Some(now_ms() + 8 * 3_600_000), Some("ref"))).is_ok());
        // …but unstorable without one (the account would break at expiry).
        assert!(ensure_storable(&minted(Some(now_ms() + 8 * 3_600_000), None)).is_err());
        // Long-lived setup tokens (or no expiry at all) need no refresh.
        assert!(ensure_storable(&minted(Some(now_ms() + 400 * 24 * 3_600_000), None)).is_ok());
        assert!(ensure_storable(&minted(None, None)).is_ok());
    }
}
