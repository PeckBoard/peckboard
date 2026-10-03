//! Device side as a reusable service: [`run_device`] is the
//! rendezvous → punch → [`connect_device`](super::connect_device) loop with
//! backoff and a cancellation token (a mobile app stops it on background and
//! restarts it on foreground), [`bind_listener`] binds the local port, and
//! [`CookieGate`] keeps other local apps off that port.

use std::fmt;
use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rand::RngCore;
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use super::{
    HEADER_TIMEOUT, PairingLink, PathKind, TunnelError, TunnelEvent, device_session, establish,
    relay_config,
};
use crate::client::ClientConfig;
use crate::proto::Role;

/// Runs on every accepted local connection before it becomes a tunnel
/// stream. Return the stream to forward it, `None` to drop it (the filter
/// may answer the connection itself first). Inspect with
/// [`TcpStream::peek`] — consumed bytes never reach the box.
pub type AcceptFilter =
    Arc<dyn Fn(TcpStream) -> Pin<Box<dyn Future<Output = Option<TcpStream>> + Send>> + Send + Sync>;

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
///
/// Returns `Ok(())` once `cancel` fires — the tunnel and every stream on it
/// are closed and `listener` is dropped (so the port stops accepting).
/// Returns `Err` only with `give_up_on_punch_failure`. To resume, bind
/// again and call `run_device` with a fresh token.
pub async fn run_device(
    opts: DeviceOptions,
    listener: TcpListener,
    cancel: CancellationToken,
    on_event: impl Fn(DeviceEvent) + Send + Sync + 'static,
) -> anyhow::Result<()> {
    let on_event = Arc::new(on_event);
    let connected_at: Arc<Mutex<Option<Instant>>> = Arc::default();
    let mut ever_connected = false;
    let mut backoff = opts.min_backoff;
    loop {
        *connected_at.lock().unwrap() = None;
        on_event(DeviceEvent::Connecting);
        let attempt = async {
            let cfg = match &opts.relay {
                Some(c) => c.clone(),
                None => relay_config(&opts.link.relay).await?,
            };
            establish(&cfg, &opts.link.secret, Role::Device).await
        };
        let path = tokio::select! {
            r = attempt => r,
            _ = cancel.cancelled() => return Ok(()),
        };
        match path {
            Ok(path) => {
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
                    &listener,
                    opts.accept_filter.as_ref(),
                    &cancel,
                    tunnel_ev,
                )
                .await;
            }
            Err(e) => match e.downcast_ref::<TunnelError>() {
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
        on_event(DeviceEvent::Retrying { after: backoff });
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = cancel.cancelled() => return Ok(()),
        }
        backoff = (backoff * 2).min(opts.max_backoff);
    }
}

// ---- cookie gate --------------------------------------------------------

/// Largest request head the gate inspects; bigger ones are dropped.
const MAX_HEAD: usize = 16 * 1024;

/// Loopback gate: only the app's own WebView may use the device port.
///
/// Holds a random key. Load [`boot_path`](Self::boot_path)
/// (`/__pbm/boot?k=<key>`) in the WebView first: the device answers it
/// itself — the box never sees it — with
/// `Set-Cookie: __pbm=<key>; HttpOnly; SameSite=Strict; Path=/` and a page
/// that replaces itself with `/` (a same-origin navigation, so the Strict
/// cookie is sent; an HTTP redirect would inherit the cross-site initiator
/// first request (HTTP or WebSocket upgrade), compared in constant time, or
/// it is dropped unanswered.
///
/// The app shell may append `&shell=<its origin, percent-encoded>` to the
/// boot path; a known Tauri shell origin (`tauri://localhost`,
/// `http(s)://tauri.localhost`, `http://localhost:<port>` in dev) is stored
/// in the readable [`SHELL_COOKIE`](Self::SHELL_COOKIE), anything else is
/// ignored.
///
/// Cookies are scoped by host, not port: create **one** gate per app launch
/// and share it (it's a cheap clone) across every paired box, or the boxes'
/// cookies overwrite each other.
#[derive(Clone)]
pub struct CookieGate {
    key: Arc<str>,
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
    /// Non-HttpOnly cookie holding the app shell's origin, so the box UI can
    /// offer a way back to the box list. Set only from a validated `shell=`
    /// boot parameter (see [`boot_response`](Self::boot_response)).
    pub const SHELL_COOKIE: &'static str = "__pbm_shell";

    /// A gate with a fresh random 256-bit key (hex).
    pub fn new() -> Self {
        let mut b = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut b);
        let key: String = b.iter().map(|x| format!("{x:02x}")).collect();
        Self { key: key.into() }
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    /// `/__pbm/boot?k=<key>` — append to `http://127.0.0.1:<port>`.
    pub fn boot_path(&self) -> String {
        format!("{}?k={}", Self::BOOT_PATH, self.key)
    }

    /// This gate as a [`DeviceOptions::accept_filter`].
    pub fn filter(&self) -> AcceptFilter {
        let gate = self.clone();
        Arc::new(move |tcp| {
            let gate = gate.clone();
            Box::pin(async move { gate.admit(tcp).await })
        })
    }

    /// Admit `tcp` (returned untouched) if its first request carries the
    /// cookie; answer a valid boot request itself; drop anything else.
    pub async fn admit(&self, mut tcp: TcpStream) -> Option<TcpStream> {
        let head = peek_head(&tcp).await?;
        let text = std::str::from_utf8(&head).ok()?;
        let mut lines = text.split("\r\n");
        let target = lines.next()?.split(' ').nth(1)?;
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        if path == Self::BOOT_PATH {
            let k = query.split('&').find_map(|kv| kv.strip_prefix("k="))?;
            if !self.matches(k) {
                return None;
            }
            let shell = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("shell="))
                .and_then(Self::shell_origin);
            let mut consumed = vec![0u8; head.len()];
            tcp.read_exact(&mut consumed).await.ok()?;
            let _ = tcp
                .write_all(self.boot_response(shell.as_deref()).as_bytes())
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
        ok.then_some(tcp)
    }

    fn matches(&self, candidate: &str) -> bool {
        candidate.as_bytes().ct_eq(self.key.as_bytes()).into()
    }

    fn boot_response(&self, shell: Option<&str>) -> String {
        let body = "<!doctype html><meta http-equiv=\"refresh\" content=\"0;url=/\">\
                    <script>location.replace(\"/\")</script>";
        let shell_cookie = shell
            .map(|s| {
                format!(
                    "Set-Cookie: {}={s}; SameSite=Strict; Path=/\r\n",
                    Self::SHELL_COOKIE
                )
            })
            .unwrap_or_default();
        format!(
            "HTTP/1.1 200 OK\r\n\
             Set-Cookie: {}={}; HttpOnly; SameSite=Strict; Path=/\r\n\
             {shell_cookie}\
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

    /// `shell=` boot parameter (percent-encoded) → the app shell's origin,
    /// only if it is one the app can actually be served from.
    fn shell_origin(raw: &str) -> Option<String> {
        let s = percent_decode(raw)?;
        let dev_port = s
            .strip_prefix("http://localhost:")
            .is_some_and(|p| (1..=5).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()));
        let known = matches!(
            s.as_str(),
            "tauri://localhost" | "http://tauri.localhost" | "https://tauri.localhost"
        );
        (known || dev_port).then_some(s)
    }
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

/// The first request head (through `\r\n\r\n`) without consuming it.
async fn peek_head(tcp: &TcpStream) -> Option<Vec<u8>> {
    let read = async {
        let mut buf = vec![0u8; MAX_HEAD];
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
            if n == MAX_HEAD {
                return None;
            }
            // `peek` returns at once while bytes are buffered; wait for more.
            if n == seen {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            seen = n;
        }
    };
    tokio::time::timeout(HEADER_TIMEOUT, read)
        .await
        .ok()
        .flatten()
}
