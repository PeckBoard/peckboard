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
//! - a [`Tunnelled`] request extension, which loopback-only routes refuse
//!   outright as a second, explicit guard.
//!
//! Per-connection tasks live in a `JoinSet` owned by the serving future,
//! so aborting the device's task (revoke / disable) drops every open
//! HTTP/WebSocket connection with it.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use tokio::net::TcpListener;
use tokio::task::JoinSet;

use super::secret::DeviceSecret;

/// Request extension present on every request that came through a relay
/// tunnel. Loopback-trusting routes must refuse requests carrying it.
#[derive(Clone, Copy, Debug)]
pub struct Tunnelled;

/// Live tunnel state reported by the relay library.
#[derive(Debug, Clone)]
pub enum TunnelUpdate {
    /// `path`: `"direct"` (hole-punched) or `"relayed"` (through the relay).
    Connected {
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
}

pub type OnRegistered = Arc<dyn Fn(Registered) + Send + Sync>;

/// The relay side of remote access: rendezvous + hole punch for one
/// pairing. A trait so the device loop is testable without the network;
/// production uses [`super::relay::RelayBackend`].
#[async_trait::async_trait]
pub trait TunnelBackend: Send + Sync + 'static {
    /// Register as the box for this pairing and return once the device
    /// has shown up and the punch succeeded. `on_registered` fires once
    /// the relay knows our endpoint.
    async fn establish(
        &self,
        relay_host: &str,
        secret: &DeviceSecret,
        direct: &DirectOptions,
        on_registered: OnRegistered,
    ) -> anyhow::Result<Box<dyn PunchedTunnel>>;
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
    /// Accept the device's QUIC connection and forward every stream to
    /// `target`. Returns when the connection ends.
    async fn serve(
        self: Box<Self>,
        secret: &DeviceSecret,
        target: SocketAddr,
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

/// Serve `app` to one tunnel until it ends. See the module docs.
pub async fn serve_tunnel(
    app: Router,
    punched: Box<dyn PunchedTunnel>,
    secret: &DeviceSecret,
    events: TunnelEvents,
) -> anyhow::Result<()> {
    use tower::Service;

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let target = listener.local_addr()?;
    let client = tunnel_client_addr(punched.peer());
    let mut make_service = app
        .layer(axum::Extension(Tunnelled))
        .into_make_service_with_connect_info::<SocketAddr>();

    let serve = punched.serve(secret, target, events);
    tokio::pin!(serve);
    let mut conns = JoinSet::new();
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
                let Ok(svc) = make_service.call(client).await;
                conns.spawn(async move {
                    let io = hyper_util::rt::TokioIo::new(tcp);
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
            _secret: &DeviceSecret,
            target: SocketAddr,
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
                punched,
                &DeviceSecret::generate(),
                Arc::new(|_| {}),
            )
            .await
            .unwrap();
            let resp = response.lock().unwrap().clone();
            assert!(resp.starts_with("HTTP/1.1 403"), "{peer}: {resp}");
            assert!(resp.contains("loopback only"), "{peer}: {resp}");
        }
    }
}
