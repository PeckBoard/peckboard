//! Box registration over HTTPS, on the signaling listener: a TLS client
//! that negotiates `http/1.1` (or no ALPN) gets exactly one HTTP/1.1
//! request answered, then the connection closes. Routes:
//!
//! - `GET /register` — the static registration page ([`PAGE`]). It reads
//!   the box key from the URL fragment (`/register#<base64url key>`), so
//!   the key never appears in a request line or log.
//! - `GET /api/register/challenge` — `{"nonce","difficulty","expires_in"}`;
//!   the nonce is single-use and short-lived.
//! - `POST /api/register` — form body `key`, `nonce`, `solution`: adds the
//!   key to the [`Registry`](crate::registry::Registry) when the proof of
//!   work checks out ([`pow_ok`]). Idempotent.
//! - `GET /api/registered?key=<base64url>` — `{"registered":bool}`.
//!
//! Everything else is a 404. Every request costs a token from a per-IP +
//! global bucket. Parsing is hand-rolled and bounded (8 KiB head, 1 KiB
//! body, one request per connection, the handshake deadline overall).

use std::net::IpAddr;
use std::time::Instant;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{debug, info, warn};

use super::{Relay, TEARDOWN_TIMEOUT, random};
use crate::identity::{decode_key, is_valid_public_key};

/// The registration page (static; no external assets).
pub const PAGE: &str = include_str!("register.html");
/// Domain separator of the proof-of-work hash.
pub const POW_PREFIX: &str = "peckrelay-register";
const MAX_HEAD: usize = 8 * 1024;
const MAX_BODY: usize = 1024;
/// Longest accepted solution (decimal counter).
const MAX_SOLUTION: usize = 20;

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
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/register") => page(),
            ("GET", "/api/register/challenge") => self.challenge(),
            ("POST", "/api/register") => self.register_box(ip, &req.body).await,
            ("GET", "/api/registered") => {
                let Some(key) = param(&req.query, "key").and_then(decode_key) else {
                    return error(400, "bad key");
                };
                let registered = s.registry.contains(&key);
                json(200, &format!("{{\"registered\":{registered}}}"))
            }
            _ => error(404, "not found"),
        }
    }

    fn challenge(&self) -> Vec<u8> {
        let s = &self.shared;
        let now = Instant::now();
        let nonce: [u8; 16] = random();
        {
            let mut ch = s.pow_challenges.lock().unwrap();
            if ch.len() >= s.cfg.registration_max_challenges {
                ch.retain(|_, exp| *exp > now);
                if ch.len() >= s.cfg.registration_max_challenges {
                    return error(503, "busy, try again later");
                }
            }
            ch.insert(nonce, now + s.cfg.registration_challenge_ttl);
        }
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
        let nonce: Option<[u8; 16]> = URL_SAFE_NO_PAD
            .decode(nonce_s)
            .ok()
            .and_then(|v| v.try_into().ok());
        // Single use: consumed whatever the outcome.
        let live = nonce.is_some_and(|n| {
            s.pow_challenges
                .lock()
                .unwrap()
                .remove(&n)
                .is_some_and(|exp| exp > Instant::now())
        });
        if !live {
            return error(400, "challenge expired, try again");
        }
        if solution.is_empty()
            || solution.len() > MAX_SOLUTION
            || !solution.bytes().all(|b| b.is_ascii_digit())
            || !pow_ok(nonce_s, key_s, solution, s.cfg.registration_pow_bits)
        {
            return error(400, "bad proof of work");
        }
        let registry = s.registry.clone();
        match tokio::task::spawn_blocking(move || registry.add(key)).await {
            Ok(Ok(new)) => {
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
            let n = URL_SAFE_NO_PAD.encode(random::<16>());
            !pow_ok(&n, key, &sol, 12)
        });
        assert!(other_nonce);
        assert_eq!(leading_zero_bits(&[0, 0x10, 0xff]), 11);
        assert_eq!(leading_zero_bits(&[0, 0]), 16);
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
