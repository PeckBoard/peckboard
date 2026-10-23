//! OAuth 2.1 `client_credentials` authorization server for Peckboard's own
//! MCP endpoint (`POST /mcp`). External systems hold a client id + secret
//! minted in Settings, swap them at `POST /oauth/token` for a short-lived
//! bearer, and call `/mcp` as the client's backing session.
//!
//! Not to be confused with `mcp_server::oauth`, which is the OAuth CLIENT
//! Peckboard uses to sign in to third-party MCP servers.
//!
//! Only SHA-256 hashes of secrets and access tokens are stored. Network
//! policy ([`RequestOrigin`]): agent tokens stay loopback-only; OAuth
//! traffic is accepted on loopback or on the HTTPS listener, never through a
//! relay tunnel, never over plain non-loopback HTTP.

use std::net::SocketAddr;

use axum::http::{HeaderMap, Uri, header};
use sha2::Digest;

/// Request extension present on every request served by the HTTPS listener
/// (`server::serve_https` layers it onto that listener's router only). The
/// plain-HTTP listener and relay tunnels never carry it.
#[derive(Clone, Copy, Debug)]
pub struct HttpsListener;

/// Lifetime of an access token minted at `/oauth/token`.
pub const ACCESS_TOKEN_TTL_SECS: i64 = 3600;
/// The single scope this server grants.
pub const SCOPE: &str = "mcp";
pub const CLIENT_ID_PREFIX: &str = "pbc_";
pub const CLIENT_SECRET_PREFIX: &str = "pbcs_";
pub const ACCESS_TOKEN_PREFIX: &str = "pbat_";

pub fn generate_client_id() -> String {
    format!(
        "{CLIENT_ID_PREFIX}{}",
        crate::service::mcp_server::oauth::random_token()
    )
}

pub fn generate_client_secret() -> String {
    format!(
        "{CLIENT_SECRET_PREFIX}{}",
        crate::service::mcp_server::oauth::random_token()
    )
}

pub fn generate_access_token() -> String {
    format!(
        "{ACCESS_TOKEN_PREFIX}{}",
        crate::service::mcp_server::oauth::random_token()
    )
}

/// SHA-256 (hex) — what `oauth_clients.secret_hash` and
/// `oauth_access_tokens.token_hash` store.
pub fn hash_secret(secret: &str) -> String {
    hex::encode(sha2::Sha256::digest(secret.as_bytes()))
}

/// Constant-time equality: the running time depends only on the lengths,
/// never on where the inputs first differ.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Does `secret` hash to `stored_hash`? Constant-time on the hashes.
pub fn verify_secret(secret: &str, stored_hash: &str) -> bool {
    ct_eq(hash_secret(secret).as_bytes(), stored_hash.as_bytes())
}

/// Where a request came from, as far as the OAuth network policy cares.
#[derive(Clone, Copy, Debug)]
pub struct RequestOrigin {
    /// Peer is 127.0.0.1 / ::1 and the request did not arrive via a tunnel.
    pub loopback: bool,
    /// Served by the HTTPS listener ([`HttpsListener`]).
    pub https: bool,
    /// Carried by a relay tunnel (`remote_access::tunnel::Tunnelled`).
    pub tunnelled: bool,
}

impl RequestOrigin {
    pub fn new(addr: SocketAddr, https: bool, tunnelled: bool) -> Self {
        Self {
            loopback: addr.ip().is_loopback() && !tunnelled,
            https,
            tunnelled,
        }
    }

    /// Agent (per-dispatch) tokens: loopback only, exactly as before OAuth.
    pub fn agent_allowed(&self) -> bool {
        self.loopback
    }

    /// OAuth token endpoint and OAuth bearers on `/mcp`: loopback, or the
    /// HTTPS listener — and never through a relay tunnel.
    pub fn oauth_allowed(&self) -> bool {
        !self.tunnelled && (self.loopback || self.https)
    }
}

/// The request's `host[:port]`: the `Host` header, else the URI authority
/// (HTTP/2 `:authority`), else `localhost:<fallback_port>`.
pub fn request_host(headers: &HeaderMap, uri: &Uri, fallback_port: u16) -> String {
    headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .filter(|h| !h.is_empty())
        .map(str::to_string)
        .or_else(|| uri.authority().map(|a| a.to_string()))
        .unwrap_or_else(|| format!("localhost:{fallback_port}"))
}

/// `host[:port]` minus the port; IPv6 literals keep their brackets.
pub fn host_without_port(host: &str) -> &str {
    if host.starts_with('[') {
        return match host.find(']') {
            Some(end) => &host[..=end],
            None => host,
        };
    }
    match host.rsplit_once(':') {
        Some((h, port)) if port.chars().all(|c| c.is_ascii_digit()) => h,
        _ => host,
    }
}

/// This server's base URL as the caller sees it: request host, `https`
/// when served by the HTTPS listener.
pub fn base_url(headers: &HeaderMap, uri: &Uri, https: bool, fallback_port: u16) -> String {
    let scheme = if https { "https" } else { "http" };
    format!("{scheme}://{}", request_host(headers, uri, fallback_port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_prefixed_hashed_and_verified() {
        let s = generate_client_secret();
        assert!(s.starts_with(CLIENT_SECRET_PREFIX));
        assert!(s.len() > 40);
        let h = hash_secret(&s);
        assert!(verify_secret(&s, &h));
        assert!(!verify_secret("pbcs_nope", &h));
        assert_ne!(generate_access_token(), generate_access_token());
    }

    #[test]
    fn network_policy() {
        let lo: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let remote: SocketAddr = "10.0.0.5:1".parse().unwrap();
        assert!(RequestOrigin::new(lo, false, false).oauth_allowed());
        assert!(RequestOrigin::new(lo, false, false).agent_allowed());
        assert!(RequestOrigin::new(remote, true, false).oauth_allowed());
        assert!(!RequestOrigin::new(remote, true, false).agent_allowed());
        assert!(!RequestOrigin::new(remote, false, false).oauth_allowed());
        assert!(!RequestOrigin::new(remote, true, true).oauth_allowed());
        assert!(!RequestOrigin::new(lo, true, true).oauth_allowed());
    }

    #[test]
    fn host_port_split() {
        assert_eq!(host_without_port("example.com:3344"), "example.com");
        assert_eq!(host_without_port("example.com"), "example.com");
        assert_eq!(host_without_port("[::1]:3344"), "[::1]");
        assert_eq!(host_without_port("[::1]"), "[::1]");
    }
}
