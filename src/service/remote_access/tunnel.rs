//! Serving one established tunnel into Peckboard's own router.
//!
//! The relay library forwards every tunnel stream to a TCP `target` on
//! this host, so tunnelled requests would naturally arrive from
//! `127.0.0.1` — and loopback-gated routes (`/mcp`) trust loopback. They
//! must not trust a remote device. So each tunnel gets its own ephemeral
//! listener on `127.0.0.1:0`, served with:
//!
//! - `ConnectInfo<SocketAddr>` = the device's real public address (see
//!   [`tunnel_client_addr`] — never a loopback address), so every
//!   `is_loopback()` check, rate limit, and auth-session IP sees the
//!   remote peer, and
//! - a [`Tunnelled`] request extension naming the device, which
//!   loopback-only routes refuse outright as a second, explicit guard, and
//!   which auth sessions created through the tunnel record so revoking the
//!   device revokes them.
//!
//! Per-connection tasks live in a `JoinSet` owned by the serving future,
//! and every connection's socket is a [`TunnelIo`] that closes once that
//! future is gone, so aborting the device's task (revoke / disable) drops
//! every open HTTP connection with it — upgraded WebSockets included,
//! which hyper hands to tasks of their own.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::Router;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinSet;

use peckboard_relay::keys::PairingSecret;

/// The box's permanent identity key (relay registration, and the box's
/// TLS identity on pairing-v2 tunnels), the relay's verdict on it for one
/// session, and the pairing-v2 credential / enrollment hook a loop serves
/// with.
pub use peckboard_relay::identity::BoxIdentity;
pub use peckboard_relay::tunnel::{BoxCredential, EnrollHandler, IdentityStatus};

/// Request extension present on every request that came through a relay
/// tunnel. Loopback-trusting routes must refuse requests carrying it.
#[derive(Clone, Debug)]
pub struct Tunnelled {
    /// The remote-access device whose tunnel carried the request.
    pub device_id: String,
}

/// Live tunnel state reported by the relay library.
#[derive(Debug, Clone)]
pub enum TunnelUpdate {
    /// `path`: `"direct"` (hole-punched) or `"relayed"` (through the relay).
    /// Repeats when the device reconnected on a new path and replaced the
    /// served connection; `peer` is then its new address.
    Connected {
        peer: SocketAddr,
        rtt_ms: u32,
        path: &'static str,
    },
    /// A relayed tunnel upgraded to direct, or fell back to the relay.
    PathChanged {
        path: &'static str,
    },
    Disconnected {
        reason: String,
    },
    Error(String),
}

pub type TunnelEvents = Arc<dyn Fn(TunnelUpdate) + Send + Sync>;

/// Direct-connection knobs for one registration: a fixed local UDP port
/// (router port-forward) and the public address to advertise it on.
#[derive(Debug, Clone, Default)]
pub struct DirectOptions {
    /// `None`: ephemeral port, nothing advertised.
    pub bind_port: Option<u16>,
    /// Host/IP to advertise `bind_port` on; `None`: the STUN-observed IP.
    pub public_host: Option<String>,
    /// Last observed public IP, so `bind_port` is advertised before STUN.
    pub public_ip_hint: Option<IpAddr>,
}

/// What a registration bound and advertised, for the status display.
#[derive(Debug, Clone)]
pub struct Registered {
    pub local_port: u16,
    pub public: SocketAddr,
    /// Every candidate sent to the device (LAN + advertised).
    pub candidates: Vec<SocketAddr>,
    /// The relay's verdict on the box identity for this session (`None`:
    /// the relay predates relay registration).
    pub identity: Option<IdentityStatus>,
}

pub type OnRegistered = Arc<dyn Fn(Registered) + Send + Sync>;

/// The relay side of remote access: rendezvous + hole punch for one
/// pairing. A trait so the device loop is testable without the network;
/// production uses [`super::relay::RelayBackend`].
#[async_trait::async_trait]
pub trait TunnelBackend: Send + Sync + 'static {
    /// Register as the box for this pairing (`secret`:
    /// [`BoxCredential::relay_secret`] — `S`, or `S_R` for an enrolled
    /// device) and return once the device has shown up and the punch
    /// succeeded. `identity` is proven to the relay so a registered box may
    /// use the relayed fallback when the relay's registration gate is on.
    /// `on_registered` fires once the relay knows our endpoint.
    async fn establish(
        &self,
        relay_host: &str,
        secret: &PairingSecret,
        direct: &DirectOptions,
        identity: Option<&BoxIdentity>,
        on_registered: OnRegistered,
    ) -> anyhow::Result<Box<dyn PunchedTunnel>>;

    /// Is box identity `key` registered with the relay? One HTTPS request;
    /// errors on relays that predate relay registration.
    async fn registration_status(&self, relay_host: &str, key: &[u8; 32]) -> anyhow::Result<bool>;
}

/// A punched path to one device, not yet serving.
#[async_trait::async_trait]
pub trait PunchedTunnel: Send {
    /// The device's address as seen on the punched path (its observed
    /// public endpoint when relayed).
    fn peer(&self) -> SocketAddr;
    /// `"direct"` or `"relayed"` (punching failed; the relay forwards the
    /// end-to-end encrypted tunnel).
    fn path(&self) -> &'static str {
        "direct"
    }
    /// The relay's verdict on the box identity when this path was set up.
    fn relay_identity(&self) -> Option<IdentityStatus> {
        None
    }
    /// Accept the device's QUIC connection under `cred` and forward every
    /// stream to `target` (what the connection may do follows from the
    /// credential and the negotiated ALPN; `enroll` answers enrollment
    /// requests and hears about activations). Returns when the connection
    /// ends (a device reconnecting on a new path meanwhile replaces it
    /// without returning).
    async fn serve(
        self: Box<Self>,
        cred: &BoxCredential,
        target: SocketAddr,
        enroll: Option<Arc<dyn EnrollHandler>>,
        events: TunnelEvents,
    ) -> anyhow::Result<()>;
}

/// The address tunnelled requests are attributed to: the device's real
/// address, canonicalised (IPv4-mapped IPv6 → IPv4) and never loopback or
/// unspecified-as-loopback — a peer that somehow appears as loopback (box
/// and device on one host) is mapped to `0.0.0.0`, which no loopback
/// check accepts.
pub fn tunnel_client_addr(peer: SocketAddr) -> SocketAddr {
    let ip = peer.ip().to_canonical();
    if ip.is_loopback() {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), peer.port())
    } else {
        SocketAddr::new(ip, peer.port())
    }
}

/// The phone app's loopback gate cookie: a device-local key, never meant
/// for the box. The app strips it from the first request on each
/// connection; this drops it from every later (keep-alive) one too, so it
/// never reaches handlers, logs or proxies.
const GATE_COOKIE: &str = "__pbm";

fn strip_gate_cookie(headers: &mut axum::http::HeaderMap) {
    use axum::http::header::COOKIE;
    let is_gate = |c: &str| {
        c.split_once('=')
            .is_some_and(|(n, _)| n.trim() == GATE_COOKIE)
    };
    let values: Vec<_> = headers.get_all(COOKIE).iter().cloned().collect();
    let has_gate = values.iter().any(|v| {
        v.to_str()
            .is_ok_and(|s| s.split(';').any(|c| is_gate(c.trim())))
    });
    if !has_gate {
        return;
    }
    headers.remove(COOKIE);
    for v in values {
        let Ok(s) = v.to_str() else {
            headers.append(COOKIE, v);
            continue;
        };
        let kept: Vec<&str> = s
            .split(';')
            .map(str::trim)
            .filter(|c| !c.is_empty() && !is_gate(c))
            .collect();
        if let Ok(v) = axum::http::HeaderValue::from_str(&kept.join("; "))
            && !kept.is_empty()
        {
            headers.append(COOKIE, v);
        }
    }
}

/// One tunnelled TCP connection, cut when its tunnel's serving future is
/// dropped (the `watch::Sender` it holds goes away): from then on reads
/// see EOF, writes fail, and the socket is closed.
struct TunnelIo {
    tcp: Option<TcpStream>,
    ended: Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl TunnelIo {
    fn new(tcp: TcpStream, mut alive: watch::Receiver<()>) -> Self {
        let ended = Box::pin(async move { while alive.changed().await.is_ok() {} });
        TunnelIo {
            tcp: Some(tcp),
            ended,
        }
    }

    /// The live socket, or `None` once the tunnel ended (closing it).
    fn live(&mut self, cx: &mut Context<'_>) -> Option<&mut TcpStream> {
        if self.tcp.is_some() && self.ended.as_mut().poll(cx).is_ready() {
            self.tcp = None;
        }
        self.tcp.as_mut()
    }
}

fn tunnel_gone() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "remote-access tunnel ended")
}

impl AsyncRead for TunnelIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut().live(cx) {
            Some(tcp) => Pin::new(tcp).poll_read(cx, buf),
            None => Poll::Ready(Ok(())),
        }
    }
}

impl AsyncWrite for TunnelIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut().live(cx) {
            Some(tcp) => Pin::new(tcp).poll_write(cx, buf),
            None => Poll::Ready(Err(tunnel_gone())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut().live(cx) {
            Some(tcp) => Pin::new(tcp).poll_flush(cx),
            None => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut().live(cx) {
            Some(tcp) => Pin::new(tcp).poll_shutdown(cx),
            None => Poll::Ready(Ok(())),
        }
    }
}

/// Serve `app` to `device_id`'s tunnel until it ends. See the module docs.
pub async fn serve_tunnel(
    app: Router,
    device_id: &str,
    punched: Box<dyn PunchedTunnel>,
    cred: &BoxCredential,
    enroll: Option<Arc<dyn EnrollHandler>>,
    events: TunnelEvents,
) -> anyhow::Result<()> {
    use tower::Service;

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let target = listener.local_addr()?;
    // Follows the device to its new address when it reconnects on a new
    // path within this tunnel (the listener above stays).
    let client = Arc::new(std::sync::Mutex::new(tunnel_client_addr(punched.peer())));
    let events: TunnelEvents = {
        let client = client.clone();
        Arc::new(move |u: TunnelUpdate| {
            if let TunnelUpdate::Connected { peer, .. } = &u {
                *client.lock().unwrap() = tunnel_client_addr(*peer);
            }
            events(u)
        })
    };
    let mut make_service = app
        .layer(axum::Extension(Tunnelled {
            device_id: device_id.to_string(),
        }))
        .layer(axum::middleware::map_request(
            |mut req: axum::extract::Request| async move {
                strip_gate_cookie(req.headers_mut());
                req
            },
        ))
        .into_make_service_with_connect_info::<SocketAddr>();

    let serve = punched.serve(cred, target, enroll, events);
    tokio::pin!(serve);
    let mut conns = JoinSet::new();
    // Never sent on; dropping it (with this future) ends every `TunnelIo`.
    let (alive, _) = watch::channel(());
    loop {
        tokio::select! {
            res = &mut serve => return res,
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
            accepted = listener.accept() => {
                let (tcp, from) = match accepted {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::debug!("remote-access accept error: {e}");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        continue;
                    }
                };
                // Only the relay library on this host dials the target.
                if !from.ip().is_loopback() {
                    continue;
                }
                let client = *client.lock().unwrap();
                let Ok(svc) = make_service.call(client).await;
                let io = TunnelIo::new(tcp, alive.subscribe());
                conns.spawn(async move {
                    let io = hyper_util::rt::TokioIo::new(io);
                    let svc = hyper_util::service::TowerToHyperService::new(svc);
                    if let Err(e) = hyper_util::server::conn::auto::Builder::new(
                        hyper_util::rt::TokioExecutor::new(),
                    )
                    .serve_connection_with_upgrades(io, svc)
                    .await
                    {
                        tracing::debug!("remote-access connection error: {e}");
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_addr_is_never_loopback() {
        for peer in ["127.0.0.1:5000", "[::1]:5000", "[::ffff:127.0.0.1]:5000"] {
            let a = tunnel_client_addr(peer.parse().unwrap());
            assert!(!a.ip().is_loopback(), "{peer} -> {a}");
        }
        let a = tunnel_client_addr("[::ffff:203.0.113.9]:4000".parse().unwrap());
        assert_eq!(a, "203.0.113.9:4000".parse().unwrap());
    }

    /// SECURITY: the phone's loopback gate key never reaches the box's
    /// handlers; other cookies (incl. the readable `__pbm_shell`) survive.
    #[test]
    fn gate_cookie_is_stripped() {
        use axum::http::{HeaderMap, HeaderValue, header::COOKIE};
        let mut h = HeaderMap::new();
        h.append(
            COOKIE,
            HeaderValue::from_static("a=b; __pbm=k; __pbm_shell=s"),
        );
        h.append(COOKIE, HeaderValue::from_static("__pbm=k"));
        strip_gate_cookie(&mut h);
        let left: Vec<_> = h.get_all(COOKIE).iter().collect();
        assert_eq!(left, ["a=b; __pbm_shell=s"]);
        let mut h = HeaderMap::new();
        h.append(COOKIE, HeaderValue::from_static("x=1;y=2"));
        strip_gate_cookie(&mut h);
        assert_eq!(h.get(COOKIE).unwrap(), "x=1;y=2");
    }

    /// A legacy (S-only) loop credential, as every pre-v2 pairing runs.
    fn legacy() -> BoxCredential {
        BoxCredential::Legacy {
            s: PairingSecret::generate(),
            identity: None,
        }
    }

    /// Fake tunnel whose "device" sends one raw HTTP request to the target
    /// — i.e. from 127.0.0.1, exactly like the relay library's forwarder.
    struct OneRequest {
        peer: SocketAddr,
        request: String,
        response: Arc<std::sync::Mutex<String>>,
    }

    #[async_trait::async_trait]
    impl PunchedTunnel for OneRequest {
        fn peer(&self) -> SocketAddr {
            self.peer
        }
        async fn serve(
            self: Box<Self>,
            _cred: &BoxCredential,
            target: SocketAddr,
            _enroll: Option<Arc<dyn EnrollHandler>>,
            _events: TunnelEvents,
        ) -> anyhow::Result<()> {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut s = tokio::net::TcpStream::connect(target).await?;
            s.write_all(self.request.as_bytes()).await?;
            let mut out = String::new();
            s.read_to_string(&mut out).await?;
            *self.response.lock().unwrap() = out;
            Ok(())
        }
    }

    /// SECURITY: a tunnelled request reaches Peckboard over loopback TCP,
    /// but must never get loopback trust — `/mcp` refuses it even when the
    /// device itself appears as a loopback peer.
    #[tokio::test]
    async fn tunnelled_requests_get_no_loopback_trust() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::auth::middleware::tests::test_state(dir.path());
        let app = crate::routes::mcp::router(state.clone()).with_state(state);
        let body = r#"{"jsonrpc":"2.0","method":"tools/list","id":1}"#;
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        for peer in ["203.0.113.5:40000", "127.0.0.1:40000"] {
            let response = Arc::new(std::sync::Mutex::new(String::new()));
            let punched = Box::new(OneRequest {
                peer: peer.parse().unwrap(),
                request: request.clone(),
                response: response.clone(),
            });
            serve_tunnel(
                app.clone(),
                "dev",
                punched,
                &legacy(),
                None,
                Arc::new(|_| {}),
            )
            .await
            .unwrap();
            let resp = response.lock().unwrap().clone();
            assert!(resp.starts_with("HTTP/1.1 403"), "{peer}: {resp}");
            assert!(resp.contains("loopback only"), "{peer}: {resp}");
        }
    }

    /// A connection handed to the router (say, an upgraded WebSocket in a
    /// task of its own) dies with the tunnel: the router side reads EOF
    /// and can't write, and the peer sees the socket close.
    #[tokio::test]
    async fn tunnel_io_ends_with_the_tunnel() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let mut peer = TcpStream::connect(l.local_addr().unwrap()).await.unwrap();
        let (tcp, _) = l.accept().await.unwrap();
        let (alive, rx) = watch::channel(());
        let mut io = TunnelIo::new(tcp, rx);
        peer.write_all(b"hi").await.unwrap();
        let mut buf = [0u8; 2];
        io.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hi");
        // Blocked mid-read when the tunnel goes, like a WebSocket handler.
        let reader = tokio::spawn(async move {
            let n = io.read(&mut [0u8; 8]).await.unwrap();
            (n, io.write_all(b"late").await.is_err())
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(alive);
        let t = std::time::Duration::from_secs(2);
        let (n, write_failed) = tokio::time::timeout(t, reader).await.unwrap().unwrap();
        assert_eq!(n, 0);
        assert!(write_failed);
        let n = tokio::time::timeout(t, peer.read(&mut [0u8; 8]))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n, 0, "peer still connected");
    }

    /// SECURITY: an auth session created through a device's tunnel
    /// (password login) records the device, and revoking the device
    /// revokes that session — and only that one.
    #[tokio::test]
    async fn revoking_a_device_revokes_sessions_created_through_it() {
        use crate::auth::middleware::tests::seed_authenticated_user;
        use crate::db::models::{NewRemoteDevice, NewUser};
        use tower::ServiceExt;

        let dir = tempfile::tempdir().unwrap();
        let state = crate::auth::middleware::tests::test_state(dir.path());
        let admin_token = seed_authenticated_user(&state, "admin").await;
        let now = chrono::Utc::now().to_rfc3339();
        state
            .db
            .create_user(NewUser {
                id: "phone-user".into(),
                username: "bob".into(),
                email: None,
                password_hash: crate::auth::password::hash_password("twelve-chars!!").unwrap(),
                role: "user".into(),
                created_at: now.clone(),
                updated_at: now.clone(),
            })
            .await
            .unwrap();
        state
            .db
            .insert_remote_device(NewRemoteDevice {
                id: "dev1".into(),
                user_id: "u1".into(),
                name: "phone".into(),
                secret_ciphertext: vec![0; 48],
                secret_nonce: vec![0; 12],
                created_at: now,
                last_connected_at: None,
            })
            .await
            .unwrap();

        let app = crate::routes::auth::router(state.clone()).with_state(state.clone());
        let body = r#"{"username":"bob","password":"twelve-chars!!"}"#;
        let request = format!(
            "POST /api/auth/login HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let response = Arc::new(std::sync::Mutex::new(String::new()));
        let punched = Box::new(OneRequest {
            peer: "203.0.113.5:40000".parse().unwrap(),
            request,
            response: response.clone(),
        });
        serve_tunnel(app, "dev1", punched, &legacy(), None, Arc::new(|_| {}))
            .await
            .unwrap();
        let resp = response.lock().unwrap().clone();
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        let sessions = state
            .db
            .list_auth_sessions_by_user("phone-user")
            .await
            .unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].remote_device_id.as_deref(), Some("dev1"));

        let revoke = axum::http::Request::builder()
            .method("DELETE")
            .uri("/api/remote-access/devices/dev1")
            .header("authorization", format!("Bearer {admin_token}"))
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = crate::routes::remote_access::router(state.clone())
            .with_state(state.clone())
            .oneshot(revoke)
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::NO_CONTENT);
        let left = state
            .db
            .list_auth_sessions_by_user("phone-user")
            .await
            .unwrap();
        assert!(left.is_empty(), "device session survived revoke");
        // The admin's own (non-tunnel) session is untouched.
        assert_eq!(
            state
                .db
                .list_auth_sessions_by_user("u1")
                .await
                .unwrap()
                .len(),
            1
        );
    }
}
