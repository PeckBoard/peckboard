//! Box registration over HTTPS, on the signaling listener: a TLS client
//! that negotiates `http/1.1` (or no ALPN) gets exactly one HTTP/1.1
//! request answered, then the connection closes. Routes:
//!
//! - `GET /register` — the static registration page ([`PAGE`]). It reads
//!   the box key from the URL fragment (`/register#<base64url key>`), so
//!   the key never appears in a request line or log.
//! - `GET /api/register/challenge` — `{"nonce","difficulty","expires_in"}`.
//!   Stateless: the nonce is its issue time plus an HMAC binding it to the
//!   client's address ([`mint_challenge`]), so there is no table to fill;
//!   it is single-use (a small per-address seen list) and short-lived.
//! - `POST /api/register` — form body `key`, `nonce`, `solution`: adds the
//!   key to the [`Registry`](crate::registry::Registry) when the proof of
//!   work checks out ([`pow_ok`]). Idempotent. New keys are capped per
//!   address per day and in total.
//! - `POST /api/registered` — JSON body `{"key":"<base64url>"}` →
//!   `{"registered":bool}` (what the page uses: the key stays out of the
//!   request line). `GET /api/registered?key=` answers the same for boxes
//!   that already poll it ([`crate::client::registration_status`]).
//!
//! Everything else is a 404. Every request costs a token from a per-IP +
//! global bucket. Parsing is hand-rolled and bounded (8 KiB head, 1 KiB
//! body, one request per connection, the handshake deadline overall).

use std::net::IpAddr;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{debug, info, warn};

use super::{Relay, TEARDOWN_TIMEOUT};
use crate::identity::{decode_key, is_valid_public_key};
use crate::limits::ip_key;
use crate::registry::Added;

/// The registration page (static; no external assets).
pub const PAGE: &str = include_str!("register.html");
/// Domain separator of the proof-of-work hash.
pub const POW_PREFIX: &str = "peckrelay-register";
const MAX_HEAD: usize = 8 * 1024;
const MAX_BODY: usize = 1024;
/// Longest accepted solution (decimal counter).
const MAX_SOLUTION: usize = 20;
/// Domain separator of the challenge MAC.
const CHALLENGE_CONTEXT: &[u8] = b"peckrelay-register-challenge";
/// Challenge bytes: issue time (unix secs, big-endian) ‖ random salt ‖
/// truncated MAC over both. The salt keeps two challenges issued to one
/// address in the same second distinct.
pub const NONCE_LEN: usize = 32;
pub type Nonce = [u8; NONCE_LEN];
const SIGNED_LEN: usize = 16;

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn challenge_mac(key: &[u8; 32], signed: &[u8], ip: IpAddr) -> [u8; NONCE_LEN - SIGNED_LEN] {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("hmac any key len");
    m.update(CHALLENGE_CONTEXT);
    m.update(signed);
    // The address the limits use: the IPv4 address or the IPv6 /64, so a
    // host's privacy-address rotation doesn't void its challenge.
    match ip_key(ip) {
        IpAddr::V4(a) => m.update(&a.octets()),
        IpAddr::V6(a) => m.update(&a.octets()),
    }
    let mut out = [0u8; NONCE_LEN - SIGNED_LEN];
    out.copy_from_slice(&m.finalize().into_bytes()[..NONCE_LEN - SIGNED_LEN]);
    out
}

/// When `nonce` claims it was issued (unix secs).
pub fn challenge_issued(nonce: &Nonce) -> u64 {
    u64::from_be_bytes(nonce[..8].try_into().expect("8 bytes"))
}

/// A challenge for `ip`, issued at `issued` (unix secs), under the relay's
/// secret `key`.
pub fn mint_challenge(key: &[u8; 32], issued: u64, ip: IpAddr) -> Nonce {
    let mut n = [0u8; NONCE_LEN];
    n[..8].copy_from_slice(&issued.to_be_bytes());
    n[8..SIGNED_LEN].copy_from_slice(&rand::random::<[u8; SIGNED_LEN - 8]>());
    let mac = challenge_mac(key, &n[..SIGNED_LEN], ip);
    n[SIGNED_LEN..].copy_from_slice(&mac);
    n
}

/// `nonce` was minted by this relay for `ip`'s address and is less than
/// `ttl_secs` old at `now` (unix secs). Single use is the caller's job.
pub fn challenge_ok(key: &[u8; 32], nonce: &Nonce, ip: IpAddr, now: u64, ttl_secs: u64) -> bool {
    let issued = challenge_issued(nonce);
    if issued > now || now - issued >= ttl_secs {
        return false;
    }
    challenge_mac(key, &nonce[..SIGNED_LEN], ip)
        .ct_eq(&nonce[SIGNED_LEN..])
        .into()
}

/// `sha256("peckrelay-register:<nonce>:<key>:<solution>")` has at least
/// `bits` leading zero bits. `nonce`/`key` are the base64url strings as
/// sent.
pub fn pow_ok(nonce: &str, key: &str, solution: &str, bits: u8) -> bool {
    let h = Sha256::digest(format!("{POW_PREFIX}:{nonce}:{key}:{solution}").as_bytes());
    leading_zero_bits(&h) >= u32::from(bits)
}

fn leading_zero_bits(h: &[u8]) -> u32 {
    let mut n = 0;
    for b in h {
        if *b == 0 {
            n += 8;
        } else {
            return n + b.leading_zeros();
        }
    }
    n
}

/// Brute-force a solution (tests, tooling). Expected work: 2^bits hashes.
pub fn pow_solve(nonce: &str, key: &str, bits: u8) -> String {
    (0u64..)
        .map(|n| n.to_string())
        .find(|s| pow_ok(nonce, key, s, bits))
        .expect("u64 range")
}

/// The `"key"` string of a small JSON object (`{"key":"<base64url>"}`).
/// Values here are base64url, which never needs JSON escapes.
fn json_key(body: &[u8]) -> Option<[u8; 32]> {
    let s = std::str::from_utf8(body).ok()?;
    let rest = &s[s.find("\"key\"")? + 5..];
    let rest = rest
        .trim_start()
        .strip_prefix(':')?
        .trim_start()
        .strip_prefix('"')?;
    decode_key(&rest[..rest.find('"')?])
}

struct Request {
    method: String,
    path: String,
    query: String,
    body: Vec<u8>,
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// One request, bounded. None ⇒ malformed / too large / EOF.
async fn read_request<S: AsyncRead + Unpin>(s: &mut S) -> Option<Request> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let head_end = loop {
        if let Some(i) = find_head_end(&buf) {
            if i > MAX_HEAD {
                return None;
            }
            break i;
        }
        if buf.len() > MAX_HEAD {
            return None;
        }
        let n = s.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buf[..head_end]).ok()?;
    let mut lines = head.split("\r\n");
    let mut rl = lines.next()?.split(' ');
    let (method, target, version) = (rl.next()?, rl.next()?, rl.next()?);
    if rl.next().is_some() || !version.starts_with("HTTP/1.") {
        return None;
    }
    let mut content_length = 0usize;
    for l in lines {
        let (name, value) = l.split_once(':')?;
        let name = name.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().ok()?;
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            return None;
        }
    }
    if content_length > MAX_BODY {
        return None;
    }
    let mut body = buf[head_end + 4..].to_vec();
    if body.len() > content_length {
        return None; // pipelining: not supported
    }
    while body.len() < content_length {
        let n = s.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..n]);
        if body.len() > content_length {
            return None;
        }
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    Some(Request {
        method: method.to_string(),
        path: path.to_string(),
        query: query.to_string(),
        body,
    })
}

/// `name=value` from a query string / form body. Values here are base64url
/// or digits, which never need percent-decoding.
fn param<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    s.split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

fn response(status: u16, content_type: &str, extra: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\nConnection: close\r\n{extra}\r\n",
        reason(status),
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

fn json(status: u16, body: &str) -> Vec<u8> {
    response(status, "application/json", "", body.as_bytes())
}

fn error(status: u16, msg: &str) -> Vec<u8> {
    // `msg` is always one of our own constant strings (no escaping needed).
    json(status, &format!("{{\"error\":\"{msg}\"}}"))
}

fn page() -> Vec<u8> {
    response(
        200,
        "text/html; charset=utf-8",
        "Content-Security-Policy: default-src 'none'; script-src 'unsafe-inline'; \
         style-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; \
         form-action 'none'; frame-ancestors 'none'\r\nX-Frame-Options: DENY\r\n",
        PAGE.as_bytes(),
    )
}

impl Relay {
    /// Answer one registration-page request on an accepted TLS stream.
    pub(super) async fn serve_http<S>(&self, mut stream: S, ip: IpAddr, deadline: Instant)
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let req = tokio::time::timeout_at(deadline.into(), read_request(&mut stream))
            .await
            .ok()
            .flatten();
        let resp = match req {
            Some(r) => self.route(ip, r).await,
            None => error(400, "bad request"),
        };
        let write_timeout = self.shared.cfg.write_timeout;
        let _ = tokio::time::timeout(write_timeout, stream.write_all(&resp)).await;
        let _ = tokio::time::timeout(TEARDOWN_TIMEOUT, stream.shutdown()).await;
    }
    async fn route(&self, ip: IpAddr, req: Request) -> Vec<u8> {
        let s = &self.shared;
        if !s.http_limiter.allow(ip) {
            return error(429, "too many requests");
        }
        debug!(peer = %self.tag(ip), method = %req.method, path = %req.path, "http");

        let registered = |key: Option<[u8; 32]>| match key {
            Some(key) => {
                let registered = s.registry.contains(&key);
                json(200, &format!("{{\"registered\":{registered}}}"))
            }
            None => error(400, "bad key"),
        };
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/register") => page(),
            ("GET", "/api/register/challenge") => self.challenge(ip),
            ("POST", "/api/register") => self.register_box(ip, &req.body).await,
            ("POST", "/api/registered") => registered(json_key(&req.body)),
            ("GET", "/api/registered") => registered(param(&req.query, "key").and_then(decode_key)),
            _ => error(404, "not found"),
        }
    }

    fn challenge(&self, ip: IpAddr) -> Vec<u8> {
        let s = &self.shared;
        let nonce = mint_challenge(&s.challenge_key, unix_secs(), ip);
        json(
            200,
            &format!(
                "{{\"nonce\":\"{}\",\"difficulty\":{},\"expires_in\":{}}}",
                URL_SAFE_NO_PAD.encode(nonce),
                s.cfg.registration_pow_bits,
                s.cfg.registration_challenge_ttl.as_secs()
            ),
        )
    }

    /// `nonce` is live for `ip` and not used before; marks it used.
    fn consume_challenge(&self, ip: IpAddr, nonce_s: &str) -> bool {
        let s = &self.shared;
        let Some(nonce) = URL_SAFE_NO_PAD
            .decode(nonce_s)
            .ok()
            .and_then(|v| Nonce::try_from(v).ok())
        else {
            return false;
        };
        let (now, ttl) = (unix_secs(), s.cfg.registration_challenge_ttl.as_secs());
        if !challenge_ok(&s.challenge_key, &nonce, ip, now, ttl) {
            return false;
        }
        let expires = challenge_issued(&nonce) + ttl;
        let mut seen = s.pow_seen.lock().unwrap();
        let used = seen.entry(ip_key(ip)).or_default();
        used.retain(|(_, exp)| *exp > now);
        if used.iter().any(|(n, _)| *n == nonce) {
            return false;
        }
        used.push((nonce, expires));
        true
    }

    async fn register_box(&self, ip: IpAddr, body: &[u8]) -> Vec<u8> {
        let s = &self.shared;
        let Ok(body) = std::str::from_utf8(body) else {
            return error(400, "bad request");
        };
        let (Some(key_s), Some(nonce_s), Some(solution)) = (
            param(body, "key"),
            param(body, "nonce"),
            param(body, "solution"),
        ) else {
            return error(400, "missing field");
        };
        let Some(key) = decode_key(key_s).filter(is_valid_public_key) else {
            return error(400, "bad key");
        };
        // Single use: consumed whatever the outcome.
        if !self.consume_challenge(ip, nonce_s) {
            return error(400, "challenge expired, try again");
        }
        if solution.is_empty()
            || solution.len() > MAX_SOLUTION
            || !solution.bytes().all(|b| b.is_ascii_digit())
            || !pow_ok(nonce_s, key_s, solution, s.cfg.registration_pow_bits)
        {
            return error(400, "bad proof of work");
        }
        // Re-registering costs nothing from the caps.
        if s.registry.contains(&key) {
            return json(200, "{\"registered\":true,\"new\":false}");
        }
        let max = s.cfg.registration_max_keys;
        if s.registry.len() >= max {
            return error(503, "registration is full, try again later");
        }
        if !s.register_limiter.allow(ip) {
            return error(429, "too many registrations from this network today");
        }
        let registry = s.registry.clone();
        match tokio::task::spawn_blocking(move || registry.add_capped(key, max)).await {
            Ok(Ok(Added::Full)) => error(503, "registration is full, try again later"),
            Ok(Ok(added)) => {
                let new = added == Added::New;
                if new {
                    info!(peer = %self.tag(ip), "box registered");
                }
                json(200, &format!("{{\"registered\":true,\"new\":{new}}}"))
            }
            Ok(Err(e)) => {
                warn!("registry write: {e}");
                error(500, "could not save, try again later")
            }
            Err(_) => error(500, "could not save, try again later"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pow_checks_bits_and_binding() {
        let (nonce, key) = ("bm9uY2Vub25jZW5vbmNl", "a2V5");
        let sol = pow_solve(nonce, key, 12);
        assert!(pow_ok(nonce, key, &sol, 12));
        assert!(pow_ok(nonce, key, &sol, 0));
        // Bound to the nonce and the key.
        let other_nonce = (0..64).any(|_| {
            let n = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 16]>());
            !pow_ok(&n, key, &sol, 12)
        });
        assert!(other_nonce);
        assert_eq!(leading_zero_bits(&[0, 0x10, 0xff]), 11);
        assert_eq!(leading_zero_bits(&[0, 0]), 16);
    }

    #[test]
    fn stateless_challenge_binds_address_key_and_time() {
        let key: [u8; 32] = rand::random();
        let ip: IpAddr = "203.0.113.7".parse().unwrap();
        let n = mint_challenge(&key, 1_000, ip);
        assert!(challenge_ok(&key, &n, ip, 1_000, 300));
        assert!(challenge_ok(&key, &n, ip, 1_299, 300));
        // Expired, or from the future.
        assert!(!challenge_ok(&key, &n, ip, 1_300, 300));
        assert!(!challenge_ok(&key, &n, ip, 999, 300));
        // Another address, another relay key, a forged time.
        assert!(!challenge_ok(
            &key,
            &n,
            "203.0.113.8".parse().unwrap(),
            1_000,
            300
        ));
        assert!(!challenge_ok(&rand::random(), &n, ip, 1_000, 300));
        assert_ne!(n, mint_challenge(&key, 1_000, ip), "same second, distinct");
        let mut later = n;
        later[..8].copy_from_slice(&1_100u64.to_be_bytes());
        assert!(!challenge_ok(&key, &later, ip, 1_100, 300));
        // IPv6: the /64 is the address (privacy addresses rotate inside it).
        let a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let n = mint_challenge(&key, 1_000, a);
        assert!(challenge_ok(
            &key,
            &n,
            "2001:db8:1:2::99".parse().unwrap(),
            1_000,
            300
        ));
        assert!(!challenge_ok(
            &key,
            &n,
            "2001:db8:1:3::1".parse().unwrap(),
            1_000,
            300
        ));
    }

    #[test]
    fn json_key_extracts_the_key() {
        let k = [7u8; 32];
        let enc = crate::identity::encode_key(&k);
        assert_eq!(
            json_key(format!("{{\"key\":\"{enc}\"}}").as_bytes()),
            Some(k)
        );
        assert_eq!(
            json_key(format!("{{ \"key\" : \"{enc}\" }}").as_bytes()),
            Some(k)
        );
        assert_eq!(json_key(b"{\"key\":\"xyz\"}"), None);
        assert_eq!(json_key(b"{}"), None);
    }

    #[tokio::test]
    async fn request_parsing_is_bounded() {
        let mut ok: &[u8] =
            b"POST /api/register?x=1 HTTP/1.1\r\nHost: a\r\nContent-Length: 3\r\n\r\nabc";
        let r = read_request(&mut ok).await.unwrap();
        assert_eq!(
            (r.method.as_str(), r.path.as_str(), r.query.as_str()),
            ("POST", "/api/register", "x=1")
        );
        assert_eq!(r.body, b"abc");
        let mut chunked: &[u8] = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert!(read_request(&mut chunked).await.is_none());
        let big = format!("GET / HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(MAX_HEAD));
        assert!(read_request(&mut big.as_bytes()).await.is_none());
        assert_eq!(param("a=1&key=xyz", "key"), Some("xyz"));
    }
}
