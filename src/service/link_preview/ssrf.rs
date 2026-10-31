//! SSRF guard for link-preview fetches.
//!
//! Every byte the preview service pulls from the network goes through
//! [`fetch`], and `fetch` only accepts a [`VettedTarget`] — a URL whose host
//! *we* resolved and whose every address passed the [`FetchPolicy`]. The
//! request then connects to that pinned address and nothing else (reqwest's
//! resolver is overridden for the host, proxies are off), so a second DNS
//! answer can't swing the connection to an internal address (no TOCTOU
//! rebinding). Redirects are never followed by reqwest: each `Location` is
//! vetted into a fresh `VettedTarget` before the next hop.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use reqwest::Url;
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, LOCATION, RANGE,
};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const TOTAL_TIMEOUT: Duration = Duration::from_secs(10);
/// A streamed body that delivers nothing for this long is abandoned.
pub const STREAM_STALL_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_REDIRECTS: usize = 5;
pub const HTML_MAX_BYTES: usize = 2 * 1024 * 1024;
pub const IMAGE_MAX_BYTES: usize = 10 * 1024 * 1024;
pub const JSON_MAX_BYTES: usize = 1024 * 1024;

const UA: &str = concat!(
    "Mozilla/5.0 (compatible; PeckboardLinkPreview/",
    env!("PECKBOARD_VERSION"),
    ")"
);

/// One admin-configured "trusted internal host": an exact host name, or an
/// IP / CIDR range. Trusted targets bypass the private-address block — for
/// link-preview fetches only ([`FetchPolicy::for_previews`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustedHost {
    Name(String),
    Cidr(IpAddr, u8),
}

impl TrustedHost {
    /// `nas.lan`, `192.168.1.5`, `10.0.0.0/8`, `fd00::/8`. `None` when the
    /// entry is neither a valid host name nor a valid IP/CIDR.
    pub fn parse(raw: &str) -> Option<Self> {
        let s = raw.trim().trim_end_matches('.').to_ascii_lowercase();
        if s.is_empty() {
            return None;
        }
        if let Some((ip, bits)) = s.split_once('/') {
            let ip: IpAddr = ip.trim_matches(['[', ']']).parse().ok()?;
            let bits: u8 = bits.parse().ok()?;
            let max = if ip.is_ipv4() { 32 } else { 128 };
            return (bits <= max).then_some(Self::Cidr(ip, bits));
        }
        if let Ok(ip) = s.trim_matches(['[', ']']).parse::<IpAddr>() {
            let bits = if ip.is_ipv4() { 32 } else { 128 };
            return Some(Self::Cidr(ip, bits));
        }
        let valid = s.len() <= 253
            && s.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            });
        valid.then_some(Self::Name(s))
    }

    fn matches_ip(&self, ip: &IpAddr) -> bool {
        let Self::Cidr(net, bits) = self else {
            return false;
        };
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(*ip),
            v4 => *v4,
        };
        match (net, ip) {
            (IpAddr::V4(n), IpAddr::V4(a)) => {
                let mask = if *bits == 0 {
                    0
                } else {
                    u32::MAX << (32 - bits)
                };
                (u32::from(*n) & mask) == (u32::from(a) & mask)
            }
            (IpAddr::V6(n), IpAddr::V6(a)) => {
                let mask = if *bits == 0 {
                    0
                } else {
                    u128::MAX << (128 - bits)
                };
                (u128::from(*n) & mask) == (u128::from(a) & mask)
            }
            _ => false,
        }
    }
}

/// Which non-public targets a fetch may reach. [`FetchPolicy::strict`]
/// reaches the public internet only.
#[derive(Debug, Clone, Default)]
pub struct FetchPolicy {
    trusted: Vec<TrustedHost>,
}

impl FetchPolicy {
    pub fn strict() -> Self {
        Self::default()
    }

    /// The link-preview policy: public internet plus the admin's trusted
    /// internal hosts. Nothing else in core builds a policy from that list.
    pub fn for_previews(trusted: Vec<TrustedHost>) -> Self {
        Self { trusted }
    }

    fn trusts_name(&self, host: &str) -> bool {
        self.trusted
            .iter()
            .any(|t| matches!(t, TrustedHost::Name(n) if n == host))
    }

    fn trusts_ip(&self, ip: &IpAddr) -> bool {
        self.trusted.iter().any(|t| t.matches_ip(ip))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    #[error("invalid url: {0}")]
    InvalidUrl(String),
    #[error("only http and https urls are permitted")]
    Scheme,
    #[error("dns resolution failed: {0}")]
    Dns(String),
    #[error("{0} resolves to a private or reserved address")]
    Blocked(String),
    #[error("too many redirects")]
    TooManyRedirects,
    #[error("timed out")]
    Timeout,
    #[error("response exceeds {0} bytes")]
    TooLarge(usize),
    #[error("unsupported content type {0:?}")]
    ContentType(String),
    #[error("upstream returned HTTP {0}")]
    Status(u16),
    #[error("request failed: {0}")]
    Request(String),
    #[error("rate limited: too many fetches to {0}")]
    RateLimited(String),
}

impl FetchError {
    /// The HTTP status the preview API answers with for this failure.
    pub fn http_status(&self) -> u16 {
        match self {
            Self::InvalidUrl(_) | Self::Scheme => 400,
            Self::Blocked(_) => 403,
            Self::ContentType(_) => 415,
            Self::RateLimited(_) => 429,
            Self::Timeout => 504,
            _ => 502,
        }
    }
}

/// Proof token: bearer's URL is http(s), carries no credentials, its host
/// was resolved by us, and every resolved address passed the
/// [`FetchPolicy`]. Only [`VettedTarget::vet`] constructs one, and
/// [`fetch`] — the only way the preview service sends a request — connects
/// to the pinned `addr` and nowhere else. See
/// `LinkPreviewService::guarded_fetch` for an example.
#[derive(Debug, Clone)]
pub struct VettedTarget {
    url: Url,
    /// `Some` for a domain host: the name reqwest's resolver is pinned for.
    domain: Option<String>,
    addr: SocketAddr,
}

impl VettedTarget {
    pub async fn vet(raw: &str, policy: &FetchPolicy) -> Result<Self, FetchError> {
        let url = Url::parse(raw.trim()).map_err(|e| FetchError::InvalidUrl(e.to_string()))?;
        Self::vet_url(url, policy).await
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Lower-cased host (no port) — the per-domain rate-limit key.
    pub fn host_key(&self) -> String {
        self.url.host_str().unwrap_or_default().to_ascii_lowercase()
    }

    async fn vet_url(mut url: Url, policy: &FetchPolicy) -> Result<Self, FetchError> {
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(FetchError::Scheme);
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(FetchError::InvalidUrl("credentials in url".into()));
        }
        url.set_fragment(None);
        let port = url
            .port_or_known_default()
            .ok_or_else(|| FetchError::InvalidUrl("no port".into()))?;
        // The URL parser canonicalises IP hosts (`0x7f.1` → `127.0.0.1`,
        // IPv6 in brackets), so a literal is recognised by parsing it back.
        let raw_host = url
            .host_str()
            .ok_or_else(|| FetchError::InvalidUrl("url has no host".into()))?
            .to_ascii_lowercase();
        let literal = raw_host.trim_matches(['[', ']']).parse::<IpAddr>().ok();
        let (domain, addrs) = match literal {
            Some(ip) => (None, vec![ip]),
            None => {
                let addrs = resolve(&raw_host, port).await?;
                (Some(raw_host), addrs)
            }
        };
        let host_label = url.host_str().unwrap_or_default().to_ascii_lowercase();
        let name_trusted = domain.as_deref().is_some_and(|d| policy.trusts_name(d));
        // Every answer must pass: a mixed public/private answer is refused
        // rather than "pick the public one", so nothing about the vetted set
        // depends on which record a resolver happens to list first.
        for ip in &addrs {
            if crate::plugin::host::is_blocked_fetch_ip(ip)
                && !name_trusted
                && !policy.trusts_ip(ip)
            {
                return Err(FetchError::Blocked(host_label));
            }
        }
        let ip = *addrs
            .first()
            .ok_or_else(|| FetchError::Dns(format!("{host_label} has no addresses")))?;
        Ok(Self {
            url,
            domain,
            addr: SocketAddr::new(ip, port),
        })
    }
}

async fn resolve(host: &str, port: u16) -> Result<Vec<IpAddr>, FetchError> {
    let lookup = tokio::net::lookup_host((host, port));
    let addrs = tokio::time::timeout(CONNECT_TIMEOUT, lookup)
        .await
        .map_err(|_| FetchError::Timeout)?
        .map_err(|e| FetchError::Dns(e.to_string()))?;
    let mut out: Vec<IpAddr> = Vec::new();
    for a in addrs {
        if !out.contains(&a.ip()) {
            out.push(a.ip());
        }
    }
    Ok(out)
}

/// What to do with a response body, decided per (lower-cased, parameter-
/// stripped) content type once the headers are in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyRule {
    /// Read up to `max` bytes; past that, truncate (`truncate`) or fail.
    Read { max: usize, truncate: bool },
    /// Stop after the headers (e.g. classify a media URL without its bytes).
    HeadersOnly,
    /// Refuse the content type.
    Refuse,
}

pub struct FetchOptions<'a> {
    pub accept: &'a str,
    /// The whole `Authorization` header value (`Bearer …`, `Basic …`).
    /// Sent on the first hop only — a redirect never carries it elsewhere.
    pub auth: Option<&'a str>,
    /// POST this JSON body on the first hop instead of a GET.
    pub post_json: Option<&'a str>,
    pub body: fn(&str) -> BodyRule,
}

impl<'a> FetchOptions<'a> {
    /// A plain unauthenticated GET.
    pub fn get(accept: &'a str, body: fn(&str) -> BodyRule) -> Self {
        Self {
            accept,
            auth: None,
            post_json: None,
            body,
        }
    }
}

#[derive(Debug)]
pub struct Fetched {
    pub final_url: Url,
    pub content_type: String,
    pub body: Vec<u8>,
    pub truncated: bool,
}

/// GET a vetted target, re-vetting every redirect hop (max
/// [`MAX_REDIRECTS`]), inside [`TOTAL_TIMEOUT`] for the whole exchange.
pub async fn fetch(
    target: VettedTarget,
    policy: &FetchPolicy,
    opts: &FetchOptions<'_>,
) -> Result<Fetched, FetchError> {
    tokio::time::timeout(TOTAL_TIMEOUT, fetch_inner(target, policy, opts))
        .await
        .map_err(|_| FetchError::Timeout)?
}

async fn fetch_inner(
    mut target: VettedTarget,
    policy: &FetchPolicy,
    opts: &FetchOptions<'_>,
) -> Result<Fetched, FetchError> {
    for hop in 0..=MAX_REDIRECTS {
        let mut resp = send_once(&target, opts, hop == 0).await?;
        let status = resp.status();
        if status.is_redirection() {
            if hop == MAX_REDIRECTS {
                return Err(FetchError::TooManyRedirects);
            }
            let loc = resp
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| FetchError::Request("redirect without location".into()))?;
            let next = target
                .url
                .join(loc)
                .map_err(|e| FetchError::InvalidUrl(e.to_string()))?;
            target = VettedTarget::vet_url(next, policy).await?;
            continue;
        }
        if !status.is_success() {
            return Err(FetchError::Status(status.as_u16()));
        }
        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let (max, truncate) = match (opts.body)(&content_type) {
            BodyRule::Refuse => return Err(FetchError::ContentType(content_type)),
            BodyRule::HeadersOnly => {
                return Ok(Fetched {
                    final_url: target.url,
                    content_type,
                    body: Vec::new(),
                    truncated: false,
                });
            }
            BodyRule::Read { max, truncate } => (max, truncate),
        };
        let declared = resp
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok());
        if !truncate && declared.is_some_and(|n| n > max) {
            return Err(FetchError::TooLarge(max));
        }
        let mut body: Vec<u8> = Vec::new();
        let mut truncated = false;
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| FetchError::Request(e.to_string()))?
        {
            let room = max.saturating_sub(body.len());
            if chunk.len() > room {
                if !truncate {
                    return Err(FetchError::TooLarge(max));
                }
                body.extend_from_slice(&chunk[..room]);
                truncated = true;
                break;
            }
            body.extend_from_slice(&chunk);
        }
        return Ok(Fetched {
            final_url: target.url,
            content_type,
            body,
            truncated,
        });
    }
    Err(FetchError::TooManyRedirects)
}
fn client_for(
    target: &VettedTarget,
    total: Option<Duration>,
) -> Result<reqwest::Client, FetchError> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        // A proxy would resolve the host itself and defeat the pin.
        .no_proxy()
        .connect_timeout(CONNECT_TIMEOUT)
        .user_agent(UA);
    builder = match total {
        Some(t) => builder.timeout(t),
        // A stream has no overall deadline, only a stall timeout.
        None => builder.read_timeout(STREAM_STALL_TIMEOUT),
    };
    if let Some(domain) = &target.domain {
        builder = builder.resolve(domain, target.addr);
    }
    builder
        .build()
        .map_err(|e| FetchError::Request(e.to_string()))
}

fn send_err(e: reqwest::Error) -> FetchError {
    if e.is_timeout() {
        FetchError::Timeout
    } else {
        FetchError::Request(e.to_string())
    }
}

async fn send_once(
    target: &VettedTarget,
    opts: &FetchOptions<'_>,
    first_hop: bool,
) -> Result<reqwest::Response, FetchError> {
    let client = client_for(target, Some(TOTAL_TIMEOUT))?;
    // A redirect after a POST continues as a GET (303 semantics).
    let mut req = match opts.post_json {
        Some(body) if first_hop => client
            .post(target.url.clone())
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_string()),
        _ => client.get(target.url.clone()),
    }
    .header(ACCEPT, opts.accept);
    if first_hop && let Some(auth) = opts.auth {
        req = req.header(AUTHORIZATION, auth);
    }
    req.send().await.map_err(send_err)
}

/// The headers of a streamed upstream response, plus the live response to
/// pull the body from ([`fetch_stream`]).
pub struct Streamed {
    pub final_url: Url,
    /// 200, or 206 for a satisfied `Range`.
    pub status: u16,
    pub content_type: String,
    pub content_length: Option<u64>,
    pub content_range: Option<String>,
    pub response: reqwest::Response,
}

/// GET a vetted target for streaming (the media proxy): redirects re-vetted
/// like [`fetch`], the client's `range` forwarded on every hop, the headers
/// awaited within [`TOTAL_TIMEOUT`] — the body then streams with only a
/// stall timeout, so a long video isn't cut off mid-play. The caller
/// enforces content type and size.
pub async fn fetch_stream(
    target: VettedTarget,
    policy: &FetchPolicy,
    accept: &str,
    range: Option<&str>,
) -> Result<Streamed, FetchError> {
    tokio::time::timeout(
        TOTAL_TIMEOUT,
        fetch_stream_inner(target, policy, accept, range),
    )
    .await
    .map_err(|_| FetchError::Timeout)?
}

async fn fetch_stream_inner(
    mut target: VettedTarget,
    policy: &FetchPolicy,
    accept: &str,
    range: Option<&str>,
) -> Result<Streamed, FetchError> {
    for hop in 0..=MAX_REDIRECTS {
        let client = client_for(&target, None)?;
        let mut req = client.get(target.url.clone()).header(ACCEPT, accept);
        if let Some(r) = range {
            req = req.header(RANGE, r);
        }
        let resp = req.send().await.map_err(send_err)?;
        let status = resp.status();
        if status.is_redirection() {
            if hop == MAX_REDIRECTS {
                return Err(FetchError::TooManyRedirects);
            }
            let loc = resp
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| FetchError::Request("redirect without location".into()))?;
            let next = target
                .url
                .join(loc)
                .map_err(|e| FetchError::InvalidUrl(e.to_string()))?;
            target = VettedTarget::vet_url(next, policy).await?;
            continue;
        }
        if status.as_u16() != 200 && status.as_u16() != 206 {
            return Err(FetchError::Status(status.as_u16()));
        }
        let header = |name| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let content_type = header(CONTENT_TYPE)
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let content_length = header(CONTENT_LENGTH).and_then(|v| v.parse().ok());
        let content_range = header(CONTENT_RANGE);
        return Ok(Streamed {
            final_url: target.url,
            status: status.as_u16(),
            content_type,
            content_length,
            content_range,
            response: resp,
        });
    }
    Err(FetchError::TooManyRedirects)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        http::{StatusCode, header},
        response::IntoResponse,
        routing::get,
    };

    /// Serve `app` on an ephemeral loopback port; returns the port.
    pub(crate) async fn serve(app: Router) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        port
    }

    fn html_rule(ct: &str) -> BodyRule {
        if ct == "text/html" {
            BodyRule::Read {
                max: 64,
                truncate: false,
            }
        } else {
            BodyRule::Refuse
        }
    }

    fn opts() -> FetchOptions<'static> {
        FetchOptions::get("text/html", html_rule)
    }

    fn trust(entries: &[&str]) -> FetchPolicy {
        FetchPolicy::for_previews(
            entries
                .iter()
                .map(|e| TrustedHost::parse(e).unwrap())
                .collect(),
        )
    }

    #[test]
    fn trusted_host_entries_parse() {
        assert_eq!(
            TrustedHost::parse("NAS.lan."),
            Some(TrustedHost::Name("nas.lan".into()))
        );
        assert_eq!(
            TrustedHost::parse("10.0.0.0/8"),
            Some(TrustedHost::Cidr("10.0.0.0".parse().unwrap(), 8))
        );
        assert_eq!(
            TrustedHost::parse("192.168.1.5"),
            Some(TrustedHost::Cidr("192.168.1.5".parse().unwrap(), 32))
        );
        assert!(TrustedHost::parse("10.0.0.0/33").is_none());
        assert!(TrustedHost::parse("bad host!").is_none());
        let net = TrustedHost::parse("10.0.0.0/8").unwrap();
        assert!(net.matches_ip(&"10.9.8.7".parse().unwrap()));
        assert!(net.matches_ip(&"::ffff:10.1.1.1".parse().unwrap()));
        assert!(!net.matches_ip(&"11.0.0.1".parse().unwrap()));
    }

    #[tokio::test]
    async fn non_http_schemes_and_credentials_are_refused() {
        let p = FetchPolicy::strict();
        for raw in ["file:///etc/passwd", "ftp://example.com/", "gopher://x/"] {
            assert_eq!(
                VettedTarget::vet(raw, &p).await.unwrap_err(),
                FetchError::Scheme,
                "{raw}"
            );
        }
        assert!(matches!(
            VettedTarget::vet("http://user:pw@example.com/", &p).await,
            Err(FetchError::InvalidUrl(_))
        ));
    }

    #[tokio::test]
    async fn private_literals_and_dns_to_private_are_blocked() {
        let p = FetchPolicy::strict();
        for raw in [
            "http://127.0.0.1/",
            "http://10.0.0.1/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://100.64.0.1/",
            "http://0.0.0.0/",
            "http://224.0.0.1/",
            "http://[::1]/",
            "http://[fd00::1]/",
            "http://[fe80::1]/",
            "http://[::ffff:10.0.0.1]/",
            // DNS → loopback.
            "http://localhost/",
        ] {
            assert!(
                matches!(
                    VettedTarget::vet(raw, &p).await,
                    Err(FetchError::Blocked(_))
                ),
                "{raw} must be blocked"
            );
        }
    }

    #[tokio::test]
    async fn trusted_hosts_bypass_only_the_preview_policy() {
        let preview = trust(&["localhost", "10.0.0.0/8"]);
        assert!(
            VettedTarget::vet("http://localhost:9/", &preview)
                .await
                .is_ok()
        );
        assert!(
            VettedTarget::vet("http://10.1.2.3/", &preview)
                .await
                .is_ok()
        );
        // Untrusted private ranges stay blocked under the preview policy …
        assert!(matches!(
            VettedTarget::vet("http://192.168.0.1/", &preview).await,
            Err(FetchError::Blocked(_))
        ));
        // … and the strict policy (every non-preview caller) ignores the list.
        assert!(matches!(
            VettedTarget::vet("http://10.1.2.3/", &FetchPolicy::strict()).await,
            Err(FetchError::Blocked(_))
        ));
    }

    #[tokio::test]
    async fn redirects_are_revetted_and_capped() {
        let app = Router::new()
            .route(
                "/ok",
                get(|| async { ([(header::CONTENT_TYPE, "text/html")], "<p>hi</p>") }),
            )
            .route(
                "/to-private",
                get(|| async {
                    (
                        StatusCode::FOUND,
                        [(header::LOCATION, "http://127.0.0.1:1/secret")],
                    )
                }),
            )
            .route(
                "/loop",
                get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/loop")]) }),
            )
            .route(
                "/hop",
                get(|| async { (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, "/ok")]) }),
            )
            .route(
                "/big",
                get(|| async { ([(header::CONTENT_TYPE, "text/html")], "x".repeat(1000)) }),
            )
            .route(
                "/json",
                get(|| async {
                    ([(header::CONTENT_TYPE, "application/json")], "{}").into_response()
                }),
            );
        let port = serve(app).await;
        // Trust the *name* only: 127.0.0.1 as a literal stays blocked.
        let p = trust(&["localhost"]);
        let url = |path: &str| format!("http://localhost:{port}{path}");

        let t = VettedTarget::vet(&url("/hop"), &p).await.unwrap();
        let got = fetch(t, &p, &opts()).await.unwrap();
        assert_eq!(got.body, b"<p>hi</p>");
        assert_eq!(got.final_url.path(), "/ok");

        let t = VettedTarget::vet(&url("/to-private"), &p).await.unwrap();
        assert!(matches!(
            fetch(t, &p, &opts()).await,
            Err(FetchError::Blocked(h)) if h == "127.0.0.1"
        ));

        let t = VettedTarget::vet(&url("/loop"), &p).await.unwrap();
        assert_eq!(
            fetch(t, &p, &opts()).await.unwrap_err(),
            FetchError::TooManyRedirects
        );

        let t = VettedTarget::vet(&url("/big"), &p).await.unwrap();
        assert_eq!(
            fetch(t, &p, &opts()).await.unwrap_err(),
            FetchError::TooLarge(64)
        );

        let t = VettedTarget::vet(&url("/json"), &p).await.unwrap();
        assert!(matches!(
            fetch(t, &p, &opts()).await,
            Err(FetchError::ContentType(ct)) if ct == "application/json"
        ));
    }
}
