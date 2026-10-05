//! Device side as a reusable service: [`run_device`] is the
//! rendezvous → punch → [`connect_device`](super::connect_device) loop with
//! backoff and a cancellation token (a mobile app stops it on background and
//! restarts it on foreground), [`bind_listener`] binds the local port, and
//! [`CookieGate`] keeps other local apps off that port.

use std::borrow::Borrow;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rand::RngCore;
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;

use super::{
    PairingLink, PathKind, TunnelError, TunnelEvent, device_session, establish, relay_config,
};
use crate::client::ClientConfig;
use crate::proto::Role;

/// Runs on every accepted local connection before it becomes a tunnel
/// stream. Return [`Admitted`] to forward it, `None` to drop it (the filter
/// may answer the connection itself first). Inspect with
/// [`TcpStream::peek`]; bytes the filter consumes reach the box only as
/// [`Admitted::head`].
pub type AcceptFilter =
    Arc<dyn Fn(TcpStream) -> Pin<Box<dyn Future<Output = Option<Admitted>> + Send>> + Send + Sync>;

/// A local connection an [`AcceptFilter`] let through: `head` goes to the
/// box first, then the rest of `tcp`.
pub struct Admitted {
    pub head: Vec<u8>,
    pub tcp: TcpStream,
}

impl Admitted {
    /// Forward `tcp` untouched.
    pub fn new(tcp: TcpStream) -> Self {
        Self {
            head: Vec::new(),
            tcp,
        }
    }
}

/// Where [`bind_listener`] binds.
#[derive(Clone, Copy, Debug)]
pub enum ListenAddr {
    /// This address or fail.
    Exact(SocketAddr),
    /// This address; a free port on the same IP if it's taken.
    Prefer(SocketAddr),
    /// A free port on 127.0.0.1.
    Ephemeral,
}

/// Bind the device's local listener. Keep the origin stable (fixed port)
/// where the web UI's per-origin storage matters.
pub async fn bind_listener(addr: ListenAddr) -> io::Result<TcpListener> {
    match addr {
        ListenAddr::Exact(a) => TcpListener::bind(a).await,
        ListenAddr::Prefer(a) => match TcpListener::bind(a).await {
            Ok(l) => Ok(l),
            Err(_) => TcpListener::bind(SocketAddr::new(a.ip(), 0)).await,
        },
        ListenAddr::Ephemeral => TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await,
    }
}

/// Configuration for [`run_device`].
#[derive(Clone)]
pub struct DeviceOptions {
    pub link: PairingLink,
    /// Relay to use instead of resolving `link.relay` (pinned cert, tests).
    pub relay: Option<ClientConfig>,
    /// See [`AcceptFilter`]; [`CookieGate::filter`] is the stock one.
    pub accept_filter: Option<AcceptFilter>,
    /// First retry delay; doubles per failure up to `max_backoff`.
    pub min_backoff: Duration,
    pub max_backoff: Duration,
    /// A tunnel that stayed up this long resets the backoff.
    pub stable_after: Duration,
    /// Return `Err(TunnelError::PunchFailed)` instead of retrying when the
    /// punch fails before any tunnel was ever established.
    pub give_up_on_punch_failure: bool,
    /// Fire [`DeviceKick::network_changed`] (a clone of this) when the OS
    /// reports a new default network.
    pub kick: DeviceKick,
}

impl DeviceOptions {
    /// Defaults: backoff 1 s → 30 s, reset after 30 s up, retry forever, no
    /// filter.
    pub fn new(link: PairingLink) -> Self {
        Self {
            link,
            relay: None,
            accept_filter: None,
            min_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
            stable_after: Duration::from_secs(30),
            give_up_on_punch_failure: false,
            kick: DeviceKick::new(),
        }
    }

    /// Gate the listener with `gate` ([`CookieGate::filter`]).
    pub fn with_gate(mut self, gate: &CookieGate) -> Self {
        self.accept_filter = Some(gate.filter());
        self
    }
}

impl fmt::Debug for DeviceOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceOptions")
            .field("link", &self.link)
            .field("accept_filter", &self.accept_filter.is_some())
            .field("min_backoff", &self.min_backoff)
            .field("max_backoff", &self.max_backoff)
            .field("stable_after", &self.stable_after)
            .field("give_up_on_punch_failure", &self.give_up_on_punch_failure)
            .finish_non_exhaustive()
    }
}

/// Kicks closer than this count once.
pub const KICK_DEBOUNCE: Duration = Duration::from_secs(1);

/// Tells a running [`run_device`] the device's network changed (Wi-Fi ↔
/// cellular): the tunnel on the old path is closed at once and a fresh
/// round (rendezvous, punch, relay fallback) starts without the retry
/// delay, instead of waiting ~15 s for the pings to time out. The listener
/// — and so the loopback port — is untouched. Kicks within
/// [`KICK_DEBOUNCE`] of the last one are ignored; with no `run_device`
/// running a kick does nothing.
#[derive(Clone, Default)]
pub struct DeviceKick(Arc<KickState>);

#[derive(Default)]
struct KickState {
    notify: Notify,
    /// Accepted kicks; a round compares it to see if one landed meanwhile.
    count: AtomicU64,
    last: Mutex<Option<Instant>>,
}

impl fmt::Debug for DeviceKick {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceKick").finish_non_exhaustive()
    }
}

impl DeviceKick {
    pub fn new() -> Self {
        Self::default()
    }

    /// The network changed; false if debounced.
    pub fn network_changed(&self) -> bool {
        {
            let mut last = self.0.last.lock().unwrap();
            if last.is_some_and(|t| t.elapsed() < KICK_DEBOUNCE) {
                return false;
            }
            *last = Some(Instant::now());
        }
        self.0.count.fetch_add(1, Ordering::SeqCst);
        self.0.notify.notify_waiters();
        true
    }

    fn count(&self) -> u64 {
        self.0.count.load(Ordering::SeqCst)
    }

    /// Resolves on the next accepted kick.
    pub(super) fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.0.notify.notified()
    }
}

/// Progress of [`run_device`], in order per attempt: `Connecting`, then
/// `Connected` … `Disconnected` or one of the failures, then `Retrying`.
#[derive(Clone, Debug)]
pub enum DeviceEvent {
    /// Starting rendezvous with the relay.
    Connecting,
    /// Tunnel up; the listener now serves the box. `path`: direct, or
    /// relayed through the rendezvous server (punching failed).
    Connected {
        peer: SocketAddr,
        rtt_ms: u32,
        path: PathKind,
    },
    /// A relayed tunnel upgraded to a direct path (or fell back).
    PathChanged { path: PathKind },
    /// Tunnel ended (`reason` is `"stopped"` after cancellation).
    Disconnected { reason: String },
    /// The box wasn't seen at the relay (offline, or the pairing revoked).
    PeerOffline,
    /// No direct path — both NATs hard — and no relay fallback (old relay
    /// or box); see [`TunnelError::PunchFailed`].
    PunchFailed { rounds: u32 },
    /// Any other failure (relay unreachable, handshake failed, local
    /// accept error).
    Failed(String),
    /// Next attempt after this delay.
    Retrying { after: Duration },
}

/// Device reconnect loop: rendezvous + punch, serve `listener` through the
/// tunnel until it drops, back off, repeat. Connections queued on the
/// listener while reconnecting are served once the next tunnel is up.
/// [`DeviceOptions::kick`] cuts a tunnel or an attempt on the old network
/// short and skips the retry delay.
///
/// Returns `Ok(())` once `cancel` fires — the tunnel and every stream on it
/// are closed. A `listener` passed by value is dropped (so the port stops
/// accepting); pass an `Arc<TcpListener>` to keep the port bound across a
/// stop and the next `run_device` (an app in the background), so no other
/// local app can take it meanwhile. Returns `Err` only with
/// `give_up_on_punch_failure`. To resume, call `run_device` again with a
/// fresh token.
pub async fn run_device<L: Borrow<TcpListener> + Send>(
    opts: DeviceOptions,
    listener: L,
    cancel: CancellationToken,
    on_event: impl Fn(DeviceEvent) + Send + Sync + 'static,
) -> anyhow::Result<()> {
    let listener: &TcpListener = listener.borrow();
    let kick = &opts.kick;
    let on_event = Arc::new(on_event);
    let connected_at: Arc<Mutex<Option<Instant>>> = Arc::default();
    let mut ever_connected = false;
    let mut backoff = opts.min_backoff;
    loop {
        *connected_at.lock().unwrap() = None;
        let kicks = kick.count();
        on_event(DeviceEvent::Connecting);
        let attempt = async {
            let cfg = match &opts.relay {
                Some(c) => c.clone(),
                None => relay_config(&opts.link.relay).await?,
            };
            establish(&cfg, &opts.link.secret, Role::Device).await
        };
        // `None`: the network changed mid-attempt; its sockets and relay
        // session belong to the old one, so start over.
        let path = tokio::select! {
            r = attempt => Some(r),
            _ = kick.notified() => None,
            _ = cancel.cancelled() => return Ok(()),
        };
        match path {
            None => {}
            Some(Ok(path)) => {
                let (ev, at) = (on_event.clone(), connected_at.clone());
                let tunnel_ev = move |e: TunnelEvent| match e {
                    TunnelEvent::Connected { peer, rtt_ms, path } => {
                        *at.lock().unwrap() = Some(Instant::now());
                        ev(DeviceEvent::Connected { peer, rtt_ms, path });
                    }
                    TunnelEvent::PathChanged { path } => ev(DeviceEvent::PathChanged { path }),
                    TunnelEvent::Disconnected { reason } => {
                        ev(DeviceEvent::Disconnected { reason })
                    }
                    TunnelEvent::Error(e) => ev(DeviceEvent::Failed(e)),
                };
                // A handshake failure was already reported as `Failed`.
                let _ = device_session(
                    path,
                    &opts.link.secret,
                    listener,
                    opts.accept_filter.as_ref(),
                    &cancel,
                    Some(kick),
                    tunnel_ev,
                )
                .await;
            }
            Some(Err(e)) => match e.downcast_ref::<TunnelError>() {
                Some(&TunnelError::PunchFailed { rounds }) => {
                    on_event(DeviceEvent::PunchFailed { rounds });
                    if opts.give_up_on_punch_failure && !ever_connected {
                        return Err(e);
                    }
                }
                Some(TunnelError::PeerOffline) => on_event(DeviceEvent::PeerOffline),
                None => on_event(DeviceEvent::Failed(format!("{e:#}"))),
            },
        }
        if cancel.is_cancelled() {
            return Ok(());
        }
        let lasted = connected_at.lock().unwrap().map(|t| t.elapsed());
        ever_connected |= lasted.is_some();
        if lasted.is_some_and(|d| d >= opts.stable_after) {
            backoff = opts.min_backoff;
        }
        // A failure on the old network says nothing about the new one.
        if kick.count() != kicks {
            backoff = opts.min_backoff;
            on_event(DeviceEvent::Retrying {
                after: Duration::ZERO,
            });
            continue;
        }
        on_event(DeviceEvent::Retrying { after: backoff });
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = kick.notified() => {
                backoff = opts.min_backoff;
                continue;
            }
            _ = cancel.cancelled() => return Ok(()),
        }
        backoff = (backoff * 2).min(opts.max_backoff);
    }
}

// ---- cookie gate --------------------------------------------------------

// ---- cookie gate --------------------------------------------------------

/// Largest request head the gate inspects; bigger ones are dropped.
const MAX_HEAD: usize = 16 * 1024;
/// First peek buffer; grows (doubling) to [`MAX_HEAD`] only while a head
/// is still arriving, so a dribbling connection holds little memory.
const FIRST_PEEK: usize = 2 * 1024;
/// How long a local connection may take to send its first request head.
/// Loopback from the app's own WebView is instant.
const GATE_HEAD_TIMEOUT: Duration = Duration::from_secs(3);
/// Local connections being gated at once; more are dropped unanswered so a
/// local app can't exhaust the app's memory or fds.
const MAX_GATING: usize = 64;

/// Loopback gate: only the app's own WebView may use the device port.
///
/// Holds a random key. Load [`boot_path`](Self::boot_path)
/// (`/__pbm/boot?k=<key>`) in the WebView first: the device answers it
/// itself — the box never sees it — with
/// `Set-Cookie: __pbm=<key>; HttpOnly; SameSite=Strict; Path=/` and a page
/// that replaces itself with `/` (a same-origin navigation, so the Strict
/// cookie is sent; an HTTP redirect would inherit the cross-site initiator
/// and the cookie would be withheld). Every other connection must carry the
/// cookie on its first request (HTTP or WebSocket upgrade), compared in
/// constant time, or it is dropped unanswered. The `__pbm` cookie is
/// removed from that first request before it is forwarded, so the box
/// doesn't learn the key.
///
/// `&next=<path, percent-encoded>` replaces `/` as the page loaded after
/// boot (a same-origin path only; see [`boot_path_to`](Self::boot_path_to)).
/// Other boot parameters are ignored, e.g. the `shell=` older app builds
/// send.
///
/// Cookies are scoped by host, not port, so every box shares the one
/// `__pbm` cookie slot: give each box its own gate and boot the WebView
/// through it whenever it switches box. Create a fresh gate whenever the
/// listener could have been exposed (app resumed from the background) and
/// re-boot the page, so a key that leaked stops working.
#[derive(Clone)]
pub struct CookieGate {
    key: Arc<str>,
    gating: Arc<Semaphore>,
}

impl fmt::Debug for CookieGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CookieGate").finish_non_exhaustive()
    }
}

impl Default for CookieGate {
    fn default() -> Self {
        Self::new()
    }
}

impl CookieGate {
    /// Cookie name.
    pub const COOKIE: &'static str = "__pbm";
    /// Path the device answers locally.
    pub const BOOT_PATH: &'static str = "/__pbm/boot";

    /// A gate with a fresh random 256-bit key (hex).
    pub fn new() -> Self {
        let mut b = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut b);
        let key: String = b.iter().map(|x| format!("{x:02x}")).collect();
        Self {
            key: key.into(),
            gating: Arc::new(Semaphore::new(MAX_GATING)),
        }
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    /// `/__pbm/boot?k=<key>` — append to `http://127.0.0.1:<port>`.
    pub fn boot_path(&self) -> String {
        format!("{}?k={}", Self::BOOT_PATH, self.key)
    }

    /// [`boot_path`](Self::boot_path) that lands on `next` (path, query and
    /// fragment of the page to restore) instead of `/`. The gate ignores a
    /// `next` that isn't a plain same-origin path.
    pub fn boot_path_to(&self, next: &str) -> String {
        format!("{}&next={}", self.boot_path(), percent_encode(next))
    }

    /// This gate as a [`DeviceOptions::accept_filter`].
    pub fn filter(&self) -> AcceptFilter {
        let gate = self.clone();
        Arc::new(move |tcp| {
            let gate = gate.clone();
            Box::pin(async move { gate.admit(tcp).await })
        })
    }

    /// Admit `tcp` if its first request carries the cookie (that head is
    /// consumed and returned without the `__pbm` cookie); answer a valid
    /// boot request itself; drop anything else.
    pub async fn admit(&self, mut tcp: TcpStream) -> Option<Admitted> {
        let head = {
            let _permit = self.gating.try_acquire().ok()?;
            peek_head(&tcp).await?
        };
        let text = std::str::from_utf8(&head).ok()?;
        let mut lines = text.split("\r\n");
        let target = lines.next()?.split(' ').nth(1)?;
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        if path == Self::BOOT_PATH {
            let k = query.split('&').find_map(|kv| kv.strip_prefix("k="))?;
            if !self.matches(k) {
                return None;
            }
            let next = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("next="))
                .and_then(Self::next_path);
            let mut consumed = vec![0u8; head.len()];
            tcp.read_exact(&mut consumed).await.ok()?;
            let _ = tcp
                .write_all(self.boot_response(next.as_deref()).as_bytes())
                .await;
            let _ = tcp.shutdown().await;
            return None;
        }
        let ok = lines
            .filter_map(|l| l.split_once(':'))
            .filter(|(name, _)| name.trim().eq_ignore_ascii_case("cookie"))
            .flat_map(|(_, v)| v.split(';'))
            .filter_map(|c| c.trim().strip_prefix("__pbm="))
            .fold(false, |acc, v| acc | self.matches(v));
        if !ok {
            return None;
        }
        let mut consumed = vec![0u8; head.len()];
        tcp.read_exact(&mut consumed).await.ok()?;
        Some(Admitted {
            head: strip_gate_cookie(text).into_bytes(),
            tcp,
        })
    }

    fn matches(&self, candidate: &str) -> bool {
        candidate.as_bytes().ct_eq(self.key.as_bytes()).into()
    }

    /// `next` must be validated by [`next_path`](Self::next_path): no
    /// quotes, `<`, `\` or whitespace, so it is inert in the HTML and JS.
    fn boot_response(&self, next: Option<&str>) -> String {
        let next = next.unwrap_or("/");
        let body = format!(
            "<!doctype html><meta http-equiv=\"refresh\" content=\"0;url={}\">\
             <script>location.replace(\"{next}\")</script>",
            next.replace('&', "&amp;")
        );
        format!(
            "HTTP/1.1 200 OK\r\n\
             Set-Cookie: {}={}; HttpOnly; SameSite=Strict; Path=/\r\n\
             Cache-Control: no-store\r\n\
             Referrer-Policy: no-referrer\r\n\
             Content-Type: text/html; charset=utf-8\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            Self::COOKIE,
            self.key,
            body.len()
        )
    }

    /// `next=` boot parameter (percent-encoded) → a same-origin path: starts
    /// with one `/` (never `//`, a scheme-relative URL), printable ASCII
    /// without quotes, `<`, `>`, `\` or backticks.
    fn next_path(raw: &str) -> Option<String> {
        let s = percent_decode(raw)?;
        let inert = s
            .bytes()
            .all(|b| b.is_ascii_graphic() && !b"\"'<>\\`".contains(&b));
        (s.starts_with('/') && !s.starts_with("//") && s.len() <= 2048 && inert).then_some(s)
    }
}

/// `head` (a full request head) without the gate's `__pbm` cookie; a
/// `Cookie` header left empty is dropped.
fn strip_gate_cookie(head: &str) -> String {
    let mut out = String::with_capacity(head.len());
    for line in head.split_inclusive("\r\n") {
        let Some((name, value)) = line.split_once(':') else {
            out.push_str(line);
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("cookie") {
            out.push_str(line);
            continue;
        }
        let kept: Vec<&str> = value
            .trim_end_matches("\r\n")
            .split(';')
            .map(str::trim)
            .filter(|c| !c.is_empty() && !c.starts_with("__pbm="))
            .collect();
        if !kept.is_empty() {
            out.push_str(&format!("{name}: {}\r\n", kept.join("; ")));
        }
    }
    out
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;

            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Percent-encode everything but unreserved characters and `/`.
fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The first request head (through `\r\n\r\n`) without consuming it.
async fn peek_head(tcp: &TcpStream) -> Option<Vec<u8>> {
    let read = async {
        let mut buf = vec![0u8; FIRST_PEEK];
        let mut seen = 0;
        loop {
            let n = tcp.peek(&mut buf).await.ok()?;
            if n == 0 {
                return None;
            }
            if let Some(i) = buf[..n].windows(4).position(|w| w == b"\r\n\r\n") {
                buf.truncate(i + 4);
                return Some(buf);
            }
            if n == buf.len() {
                if n >= MAX_HEAD {
                    return None;
                }
                buf.resize((n * 2).min(MAX_HEAD), 0);
                continue;
            }
            // `peek` returns at once while bytes are buffered; wait for more.
            if n == seen {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            seen = n;
        }
    };
    tokio::time::timeout(GATE_HEAD_TIMEOUT, read)
        .await
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kicks_are_debounced_and_shared_by_clones() {
        let kick = DeviceKick::new();
        let clone = kick.clone();
        assert!(kick.network_changed());
        assert!(!clone.network_changed(), "second kick within the debounce");
        assert_eq!(kick.count(), 1);
        *kick.0.last.lock().unwrap() = Some(Instant::now() - KICK_DEBOUNCE);
        assert!(clone.network_changed());
        assert_eq!(kick.count(), 2);
    }
    #[test]
    fn gate_cookie_is_stripped_from_the_forwarded_head() {
        let head = "GET / HTTP/1.1\r\nHost: x\r\nCookie: a=b; __pbm=k; __pbm_shell=s\r\n\
                    cookie: __pbm=k\r\nX: __pbm=k\r\n\r\n";
        assert_eq!(
            strip_gate_cookie(head),
            "GET / HTTP/1.1\r\nHost: x\r\nCookie: a=b; __pbm_shell=s\r\nX: __pbm=k\r\n\r\n"
        );
    }

    #[test]
    fn next_path_only_allows_inert_same_origin_paths() {
        let gate = CookieGate::new();
        let boot = gate.boot_path_to("/sessions/a b?x=1&y=2#t");
        let raw = boot.split("&next=").nth(1).unwrap();
        assert_eq!(
            CookieGate::next_path(raw).as_deref(),
            None,
            "space is not inert"
        );
        let boot = gate.boot_path_to("/sessions/a?x=1&y=2#t");
        let raw = boot.split("&next=").nth(1).unwrap();
        assert!(!raw.contains('&') && !raw.contains('#'));
        assert_eq!(
            CookieGate::next_path(raw).as_deref(),
            Some("/sessions/a?x=1&y=2#t")
        );
        for bad in [
            "//evil.example/",
            "https://evil.example/",
            "/\\evil",
            "/a\"onload=x",
            "/</script>",
            "javascript:alert(1)",
            "",
        ] {
            assert_eq!(CookieGate::next_path(&percent_encode(bad)), None, "{bad}");
        }
    }
}
