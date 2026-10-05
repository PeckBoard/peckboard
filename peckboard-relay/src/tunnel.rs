//! Box↔device data path (`tunnel` feature).
//!
//! After rendezvous + hole punch ([`establish`]) the two peers share one UDP
//! 5-tuple. QUIC (quinn) runs over that very socket — reusing it keeps the
//! NAT mapping the punch opened. The box is the QUIC server, the device the
//! client; TLS 1.3 inside QUIC is mutually authenticated against Ed25519
//! keys derived from the pairing secret `S` (labels distinct from the relay
//! auth and E2E keys), with no CA involved. ALPN [`TUNNEL_ALPN`]. When no
//! direct path can be punched, the same QUIC runs over the relay instead
//! ([`RelayedPath`], [`PathKind::Relayed`]); the relay only forwards its
//! ciphertext.
//!
//! Every bidirectional stream is one forwarded TCP connection: a 1-byte
//! type header ([`STREAM_TCP`]) followed by raw bytes. The box pipes it to
//! its configured local target; the device has no way to choose that
//! target. HTTP, WebSocket and uploads all pass through untouched.
//! Pairing v2 ([`cred`]): a v2 link pins the box identity key `B` and is
//! good for one enrollment ([`STREAM_ENROLL`], ALPN [`TUNNEL_ALPN_V2`]);
//! afterwards the tunnel is authenticated by `B` and the device's own key,
//! and the rendezvous runs on a secret only the enrolled device holds. The
//! credential a loop runs with ([`BoxCredential`], [`DeviceCredential`])
//! picks the ALPN, the certificates and what a connection may do:
//!
//! | box loop | ALPN | connection may |
//! | --- | --- | --- |
//! | `Legacy` | `/1` | forward + ping; upgrade (`0x03` mode 2) with an identity |
//! | `Link(Enroll)` | `/2` | ping + enroll (mode 1); closed `enrolled` after the ack |
//! | `Link(Enroll)` | `/1` | nothing: closed `update-app` |
//! | `Link(Refuse(r))` | `/2` | ping; every enrollment refused with `r` |
//! | `Link(Refuse(r))` | `/1` | nothing: closed `link-used` / `link-expired` |
//! | `Enrolled` | `/2` | forward + ping |
//!
//!
//! ```no_run
//! # async fn demo(secret: peckboard_relay::keys::PairingSecret) -> anyhow::Result<()> {
//! use peckboard_relay::proto::Role;
//! use peckboard_relay::tunnel::{establish, relay_config, serve_box};
//! let cfg = relay_config("relay.peckboard.com").await?;
//! loop {
//!     let path = establish(&cfg, &secret, Role::Box).await?;
//!     serve_box(path, &secret, "127.0.0.1:3344".parse()?, |ev| println!("{ev:?}")).await?;
//! }
//! # }
//! ```

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use hkdf::Hkdf;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Connection, Endpoint, EndpointConfig, TransportConfig, VarInt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use sha2::Sha256;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, watch};

pub use crate::client::IdentityStatus;
use crate::client::{ClientConfig, Event, RelayClient};
pub use crate::identity::BoxIdentity;
use crate::keys::PairingSecret;
use crate::proto::Role;

pub mod cred;
mod device;
pub mod enroll;
mod relayed;
/// Re-exported so [`EnrollHandler`] implementors need no direct dep.
pub use async_trait::async_trait;
pub use cred::{
    BoxCredential, DeviceCredential, EnrollMode, EnrolledCredential, HTTPS_LINK_PREFIX,
    LINK_PREFIX, LINK_VERSION, LinkMode, PairingLink, RefuseReason,
};
pub use device::{
    AcceptFilter, Admitted, CookieGate, DeviceEvent, DeviceKick, DeviceOptions, KICK_DEBOUNCE,
    ListenAddr, OnEnrolled, bind_listener, run_device,
};
pub use enroll::EnrollHandler;
pub use quinn;
pub use relayed::RelayedPath;
/// Stops [`run_device`]; re-exported so callers need no `tokio-util` dep.
pub use tokio_util::sync::CancellationToken;

/// ALPN of the box↔device QUIC connection with `S`-derived certificates
/// (pairings from before v2).
pub const TUNNEL_ALPN: &[u8] = b"peckboard-tunnel/1";
/// ALPN of a pairing-v2 connection: box cert is the box identity `B`.
pub const TUNNEL_ALPN_V2: &[u8] = b"peckboard-tunnel/2";
/// Stream type: forward to the box's configured local TCP target.
pub const STREAM_TCP: u8 = 0x01;
/// Stream type: liveness ping. The device writes one byte every 5 s, the
/// box echoes it; either side drops the tunnel after 3 silent intervals.
/// Peers that predate it reset the stream and fall back to the idle timeout.
pub const STREAM_PING: u8 = 0x02;
/// Stream type: pairing-v2 enrollment (see [`enroll`]). Boxes that
/// predate it reset the stream.
pub const STREAM_ENROLL: u8 = 0x03;
/// Application close code: enrollment done, reconnect with the new
/// credential (reason `enrolled`).
pub const CLOSE_ENROLLED: u32 = 0x10;
/// Application close code: this connection is refused (reason
/// `update-app`, `link-used`, `link-expired` or `not-enrollable`).
pub const CLOSE_REFUSED: u32 = 0x11;
/// Default rendezvous server.
pub const DEFAULT_RELAY: &str = "relay.peckboard.com";

const HKDF_SALT: &[u8] = b"peckboard-relay/v1";
const SERVER_NAME: &str = "peckboard-tunnel";
const KEEPALIVE: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// App-level liveness ([`STREAM_PING`]): a dead peer is noticed after
/// `PING_EVERY * PING_MISSES` instead of [`IDLE_TIMEOUT`].
const PING_EVERY: Duration = Duration::from_secs(5);
const PING_MISSES: u32 = 3;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// An enrollment-only connection is closed after this long.
const ENROLL_ONLY_MAX: Duration = Duration::from_secs(60);
/// Concurrent box-side handshakes from the expected peer address (only a
/// spoofer or a retrying device makes more than one).
const MAX_HANDSHAKES: usize = 8;
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const PUNCH_TIMEOUT: Duration = Duration::from_secs(5);
/// Failed punch rounds before [`TunnelError::PunchFailed`].
const MAX_PUNCH_FAILURES: u32 = 4;
/// How long a device waits for its box to show up at the relay.
const DEVICE_PEER_WAIT: Duration = Duration::from_secs(30);
/// After a failed round, how long to wait for the next one.
const RETRY_WAIT: Duration = Duration::from_secs(10);
/// Re-STUN + relay ping while waiting: keeps the NAT mapping and the relay
/// session (90 s idle timeout) alive and the recorded endpoint current.
const MAINTAIN_EVERY: Duration = Duration::from_secs(20);
/// Refresh the STUN credential when it has less than this left.
const CRED_MARGIN: Duration = Duration::from_secs(90);
/// Wait this long for the relay's `Pong` in [`relay_sync`].
const SYNC_TIMEOUT: Duration = Duration::from_secs(5);

/// Errors from [`establish`] a caller may want to tell apart (downcast the
/// `anyhow::Error`).
#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    /// Both peers were online but no punch round got through — typically
    /// both sides are behind symmetric / hard NATs.
    #[error("hole punch failed after {rounds} rounds")]
    PunchFailed { rounds: u32 },
    /// The other side never showed up at the relay (device side only).
    #[error("peer is not online")]
    PeerOffline,
    /// The box refused to enroll this device with its v2 link, for a
    /// reason retrying won't fix ([`RefuseReason::is_final`]).
    #[error("{reason}")]
    EnrollRefused { reason: RefuseReason },
}

/// The punched (or relayed) path, ready for [`serve_box`] /
/// [`connect_device`].
pub struct PunchedPath {
    pub socket: PathSocket,
    /// The peer's address as seen from `socket` (direct), or its
    /// STUN-observed endpoint (relayed).
    pub peer: SocketAddr,
    pub role: Role,
    /// What the relay said about our box identity when this path was set
    /// up (`None`: the relay predates identities). See [`IdentityStatus`].
    pub relay_identity: Option<IdentityStatus>,
}

impl PunchedPath {
    /// Which path the tunnel starts on.
    pub fn kind(&self) -> PathKind {
        self.socket.kind()
    }

    /// The path in use and its changes (a relayed path that upgrades to
    /// direct, or falls back). A direct path never changes.
    pub fn path_watch(&self) -> watch::Receiver<PathKind> {
        match &self.socket {
            PathSocket::Direct(_) => watch::channel(PathKind::Direct).1,
            PathSocket::Relayed(r) => r.path_watch(),
        }
    }
}

/// What carries the tunnel's QUIC datagrams.
#[derive(Debug)]
pub enum PathSocket {
    /// The exact socket the punch used (same local port ⇒ same NAT mapping).
    Direct(UdpSocket),
    /// The relay session's data channel (punching failed).
    Relayed(RelayedPath),
}

impl PathSocket {
    pub fn kind(&self) -> PathKind {
        match self {
            PathSocket::Direct(_) => PathKind::Direct,
            PathSocket::Relayed(_) => PathKind::Relayed,
        }
    }

    /// Local address of the UDP socket the punch used.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        match self {
            PathSocket::Direct(s) => s.local_addr(),
            PathSocket::Relayed(r) => r.local_addr(),
        }
    }
}

impl From<UdpSocket> for PathSocket {
    fn from(s: UdpSocket) -> Self {
        PathSocket::Direct(s)
    }
}

/// How the tunnel's packets travel, for status displays.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PathKind {
    /// Peer to peer over the hole-punched UDP path.
    Direct,
    /// Through the rendezvous server (end-to-end encrypted QUIC; the relay
    /// only forwards ciphertext).
    Relayed,
}

impl PathKind {
    /// `"direct"` / `"relayed"`.
    pub fn as_str(self) -> &'static str {
        match self {
            PathKind::Direct => "direct",
            PathKind::Relayed => "relayed",
        }
    }
}

impl fmt::Display for PathKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug)]
pub enum TunnelEvent {
    Connected {
        peer: SocketAddr,
        rtt_ms: u32,
        path: PathKind,
    },
    /// A relayed tunnel switched to a direct path or back.
    PathChanged {
        path: PathKind,
    },
    Disconnected {
        reason: String,
    },
    Error(String),
}
/// Resolve `host[:port]` (default port 443) to a webpki-trusting
/// [`ClientConfig`]. Prefers IPv4 (STUN + punching are address-family
/// bound and most NATs are v4).
pub async fn relay_config(host: &str) -> anyhow::Result<ClientConfig> {
    let (name, port) = match host.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => (h, p.parse::<u16>().context("relay port")?),
        _ => (host, 443),
    };
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((name, port))
        .await
        .with_context(|| format!("resolve {name}"))?
        .collect();
    let addr = addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or(addrs.first())
        .copied()
        .ok_or_else(|| anyhow!("{name} has no addresses"))?;
    Ok(ClientConfig::webpki(addr, name))
}

/// The relay's box registration page for identity `key`:
/// `https://<relay_host>/register#<base64url key>`. The key travels in the
/// fragment, so it never reaches a server log.
pub fn registration_url(relay_host: &str, key: &[u8; 32]) -> String {
    format!(
        "https://{}/register#{}",
        relay_host.trim_end_matches('/'),
        crate::identity::encode_key(key)
    )
}

/// Is box identity `key` registered with the relay at `host[:port]`? One
/// HTTPS request ([`crate::client::registration_status`]); poll it while
/// the user registers, then reconnect (a session's
/// [`PunchedPath::relay_identity`] is fixed at handshake time).
pub async fn registration_status(relay_host: &str, key: &[u8; 32]) -> anyhow::Result<bool> {
    let cfg = relay_config(relay_host).await?;
    crate::client::registration_status(&cfg, key).await
}

// ---- rendezvous + punch -------------------------------------------------

/// An extra address to advertise to the peer as a punch candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Advertise {
    /// A known public endpoint, e.g. a router port-forward to this host.
    Addr(SocketAddr),
    /// This port on the public IP the relay's STUN observes (or
    /// [`EstablishOptions::public_ip_hint`] until it has) — for a
    /// port-forward when the public IP isn't configured.
    Port(u16),
}

/// What [`establish_with`] registered, for status displays.
#[derive(Clone, Debug)]
pub struct Registration {
    pub local_port: u16,
    /// Our endpoint as the relay's STUN sees it.
    pub public: SocketAddr,
    /// Every candidate sent to the peer (LAN + advertised).
    pub candidates: Vec<SocketAddr>,
    /// The relay's verdict on our box identity (`None`: relay predates
    /// identities).
    pub identity: Option<IdentityStatus>,
}

pub type OnRegistered = Arc<dyn Fn(&Registration) + Send + Sync>;

/// Knobs for [`establish_with`]; the default is what [`establish`] does.
#[derive(Clone, Default)]
pub struct EstablishOptions {
    /// Bind the punch socket to this local UDP port (`None`: ephemeral).
    /// With a router port-forward to it, a box behind a symmetric NAT is
    /// still reachable: the peer probes the forwarded [`Advertise`] address.
    pub bind_port: Option<u16>,
    /// Sent to the peer next to the LAN candidate; the punch probes them
    /// like any other candidate. Addresses of the other IP family than the
    /// relay are skipped.
    pub advertise: Vec<Advertise>,
    /// Public IP to resolve [`Advertise::Port`] with before STUN answers
    /// (e.g. the last [`Registration::public`]). Without it the port
    /// candidate is sent after the STUN Binding, so a peer already waiting
    /// may get one punch round without it.
    pub public_ip_hint: Option<IpAddr>,
    pub on_registered: Option<OnRegistered>,
    /// Relaying through the rendezvous server when punching fails.
    /// Relaying through the rendezvous server when punching fails.
    pub fallback: RelayFallback,
    /// Box side: the box's permanent identity, proven to a v3 relay so it
    /// may relay while the relay's registration gate is on. When the relay
    /// reports it unregistered on a gated relay, the box doesn't offer the
    /// relay fallback (a failed punch is [`TunnelError::PunchFailed`]).
    /// Ignored for devices.
    pub identity: Option<BoxIdentity>,
    /// Don't offer the LAN candidate (tests: on loopback it would always
    /// punch).
    #[doc(hidden)]
    pub no_lan_candidate: bool,
}

impl fmt::Debug for EstablishOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EstablishOptions")
            .field("bind_port", &self.bind_port)
            .field("advertise", &self.advertise)
            .field("public_ip_hint", &self.public_ip_hint)
            .field("fallback", &self.fallback)
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

/// Relay fallback policy. Takes effect only when the relay and both peers
/// speak protocol v2; with an older relay or peer, a failed punch is
/// [`TunnelError::PunchFailed`] as before.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelayFallback {
    /// Relay when punching fails (default on).
    pub enabled: bool,
    /// Failed punch rounds before relaying (default 2).
    pub after_failures: u32,
    /// Box side, while relayed: ask for a punch round to upgrade to a
    /// direct path after this long, doubling up to 10 min (default 30 s;
    /// `None`: stay relayed). The device always joins rounds it is sent.
    pub upgrade_every: Option<Duration>,
}

impl Default for RelayFallback {
    fn default() -> Self {
        Self {
            enabled: true,
            after_failures: 2,
            upgrade_every: Some(Duration::from_secs(30)),
        }
    }
}

/// LAN candidate plus advertised addresses (same family as the relay),
/// deduplicated. `Port` entries resolve against `public_ip`, and are left
/// out while it is unknown; returns whether any were left out.
fn candidates(
    lan: Option<SocketAddr>,
    advertise: &[Advertise],
    public_ip: Option<IpAddr>,
    v4: bool,
) -> (Vec<SocketAddr>, bool) {
    let mut out: Vec<SocketAddr> = lan.into_iter().collect();
    let mut pending = false;
    for a in advertise {
        let addr = match *a {
            Advertise::Addr(a) => a,
            Advertise::Port(p) => match public_ip {
                Some(ip) => SocketAddr::new(ip, p),
                None => {
                    pending = true;
                    continue;
                }
            },
        };
        if addr.is_ipv4() == v4 && !out.contains(&addr) {
            out.push(addr);
        }
    }
    (out, pending)
}

/// Run rendezvous + hole punch for one pairing and return the punched path.
///
/// The box side waits indefinitely for its device (drop the future to
/// cancel); the device side gives up with [`TunnelError::PeerOffline`] if
/// the box isn't seen within 30 s. When punch rounds keep failing and both
/// peers and the relay support it (protocol v2), both switch to a
/// [`PathKind::Relayed`] path through the relay instead (see
/// [`RelayFallback`]); otherwise either side returns
/// [`TunnelError::PunchFailed`]. A direct path closes the relay session on
/// return — a reconnect is a fresh `establish`; a relayed one keeps it.
pub async fn establish(
    cfg: &ClientConfig,
    secret: &PairingSecret,
    role: Role,
) -> anyhow::Result<PunchedPath> {
    establish_with(cfg, secret, role, &EstablishOptions::default()).await
}

/// [`establish`] with a fixed local port and/or extra advertised
/// candidates (see [`EstablishOptions`]).
pub async fn establish_with(
    cfg: &ClientConfig,
    secret: &PairingSecret,
    role: Role,
    opts: &EstablishOptions,
) -> anyhow::Result<PunchedPath> {
    let identity = (role == Role::Box)
        .then_some(opts.identity.as_ref())
        .flatten();
    let mut relay = RelayClient::connect_with_identity(cfg, secret, role, identity)
        .await
        .context("connect to relay")?;
    let relay_identity = relay.identity_status();
    let v4 = cfg.relay.is_ipv4();
    let bind: SocketAddr = if v4 {
        "0.0.0.0:0".parse()?
    } else {
        "[::]:0".parse()?
    };
    let bind = SocketAddr::new(bind.ip(), opts.bind_port.unwrap_or(0));
    let sock = UdpSocket::bind(bind)
        .await
        .with_context(|| format!("bind UDP {bind}"))?;
    let port = sock.local_addr()?.port();
    let lan = (!opts.no_lan_candidate)
        .then(|| local_ip_toward(cfg.relay).map(|ip| SocketAddr::new(ip, port)))
        .flatten();
    let (mut cands, pending) = candidates(lan, &opts.advertise, opts.public_ip_hint, v4);
    // Events that arrive during a sync (`PeerOnline`, the peer's
    // RELAY_CAP, a punch round) are replayed into the loop below; dropping
    // them left the later-registering side unable to fall back to the relay.
    let mut backlog = std::collections::VecDeque::new();
    if !cands.is_empty() {
        relay.set_candidates(&cands).await?;
        // The STUN Binding below (UDP) is what lets the relay coordinate the
        // punch; it can overtake the candidates frame (TLS). Then the peer
        // is told to punch without our LAN address and, where the LAN path
        // is the only one that works back to us, the punch is one-sided:
        // the peer "succeeds", we time out, and the reconnect costs ~15 s.
        backlog.extend(relay_sync(&mut relay).await?);
    }
    let public = relay.stun_binding(&sock).await.context("relay STUN")?;
    let (resolved, _) = candidates(lan, &opts.advertise, Some(public.ip()), v4);
    if pending || resolved != cands {
        // `Port` candidates needed the STUN-observed IP (no hint, or a
        // stale one). Same barrier as above before the next punch round;
        // a round the STUN Binding already triggered is kept, not lost.
        cands = resolved;
        relay.set_candidates(&cands).await?;
        backlog.extend(relay_sync(&mut relay).await?);
    }
    tracing::debug!(?role, ?cands, %public, "establish: registered");
    if let Some(cb) = &opts.on_registered {
        cb(&Registration {
            local_port: port,
            public,
            candidates: cands.clone(),
            identity: relay_identity,
        });
    }
    // Relay fallback needs protocol v2 on both sessions: we know ours, the
    // peer tells us with a sealed RELAY_CAP once both are online. A box the
    // relay won't relay for (gated, identity unregistered) never offers it,
    // so the device doesn't try either. (A device's own status says nothing
    // about its box.)
    let gate_ok = role != Role::Box || relay_identity.is_none_or(|s| s.relay_permitted());
    if !gate_ok {
        tracing::info!("tunnel: relay fallback unavailable, box identity not registered");
    }
    let relay_ok = opts.fallback.enabled && relay.protocol_version() >= 2 && gate_ok;
    let mut peer_relay = false;
    let mut cap_sent = false;
    let mut peer_public: Option<SocketAddr> = None;
    let mut failures = 0u32;
    let mut deadline =
        (role == Role::Device).then(|| tokio::time::Instant::now() + DEVICE_PEER_WAIT);
    let mut tick =
        tokio::time::interval_at(tokio::time::Instant::now() + MAINTAIN_EVERY, MAINTAIN_EVERY);
    let mut refreshing = false;
    loop {
        let timeout = async {
            match deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = timeout => {
                if failures > 0 {
                    if relay_ok && peer_relay {
                        return go_relayed(relay, sock, peer_public, cfg, role, opts, true).await;
                    }
                    tracing::info!(?role, failures, relay_ok, peer_relay, "tunnel: punching failed, no relay fallback");
                    return Err(TunnelError::PunchFailed { rounds: failures }.into());
                }
                return Err(TunnelError::PeerOffline.into());
            }
            _ = tick.tick() => {
                relay.ping().await?;
                let cred = relay.stun_credential();
                if cred.expires.saturating_duration_since(std::time::Instant::now()) < CRED_MARGIN {
                    relay.refresh_stun().await?;
                    refreshing = true;
                } else {
                    let _ = relay.stun_binding(&sock).await;
                }
            }
            ev = async {
                match backlog.pop_front() {
                    Some(ev) => Some(ev),
                    None => relay.next_event().await,
                }
            } => match ev {
                None => bail!("relay connection lost"),
                Some(Event::CredentialRefreshed) if refreshing => {
                    refreshing = false;
                    let _ = relay.stun_binding(&sock).await;
                }
                Some(Event::PeerOnline) if relay_ok => {
                    relay.send(RELAY_CAP).await?;
                    cap_sent = true;
                }
                Some(Event::Message { plaintext, .. }) if relay_ok => {
                    if plaintext == RELAY_CAP {
                        peer_relay = true;
                        // Backstop: a peer that registered after us may never
                        // have seen our announcement.
                        if !cap_sent {
                            relay.send(RELAY_CAP).await?;
                            cap_sent = true;
                        }
                    } else if plaintext == RELAY_GO {
                        // The peer gave up punching and is relaying now.
                        return go_relayed(relay, sock, peer_public, cfg, role, opts, false).await;
                    }
                }
                Some(Event::Punch(p)) => {
                    peer_public = Some(p.peer_public);
                    match relay.punch(&sock, &p, PUNCH_TIMEOUT).await {
                        Ok(peer) => {
                            return Ok(PunchedPath {
                                socket: sock.into(),
                                peer,
                                role,
                                relay_identity,
                            });
                        }
                        Err(_) => {
                            failures += 1;
                            if relay_ok && peer_relay && failures >= opts.fallback.after_failures {
                                return go_relayed(relay, sock, peer_public, cfg, role, opts, true)
                                    .await;
                            }
                            if failures >= MAX_PUNCH_FAILURES {
                                tracing::info!(?role, failures, relay_ok, peer_relay, "tunnel: punching failed, no relay fallback");
                                return Err(TunnelError::PunchFailed { rounds: failures }.into());
                            }
                            deadline = Some(tokio::time::Instant::now() + RETRY_WAIT);
                            let _ = relay.request_punch().await;
                        }
                    }
                }
                Some(_) => {}
            }
        }
    }
}

/// Sealed peer message: "I can relay" (protocol v2 session, fallback on).
const RELAY_CAP: &[u8] = b"pb/relay-cap/1";
/// Sealed peer message: "punching failed, I'm switching to the relay".
const RELAY_GO: &[u8] = b"pb/relay-go/1";

/// Switch to the relayed path, keeping the relay session (it carries the
/// data) and the punch socket (for upgrades). `announce`: tell the peer to
/// switch too (not needed when it told us).
#[allow(clippy::too_many_arguments)]
async fn go_relayed(
    mut relay: RelayClient,
    sock: UdpSocket,
    peer_public: Option<SocketAddr>,
    cfg: &ClientConfig,
    role: Role,
    opts: &EstablishOptions,
    announce: bool,
) -> anyhow::Result<PunchedPath> {
    if announce {
        relay.send(RELAY_GO).await?;
    }
    let data = relay
        .take_data()
        .ok_or_else(|| anyhow!("relay session has no data channel"))?;
    tracing::info!(
        ?role,
        "tunnel: no direct path, relaying through the rendezvous server"
    );
    let relay_identity = relay.identity_status();
    Ok(PunchedPath {
        socket: PathSocket::Relayed(RelayedPath::new(
            relay,
            data,
            sock,
            opts.fallback.upgrade_every,
        )),
        // QUIC's name for the peer; the relay path ignores it. Its observed
        // endpoint is the most useful thing to show (and to rate-limit by).
        // A dual-stack relay reports v4 peers v4-mapped, which quinn rejects
        // on a v4 socket.
        peer: {
            let a = peer_public.unwrap_or(cfg.relay);
            SocketAddr::new(a.ip().to_canonical(), a.port())
        },
        role,
        relay_identity,
    })
}

/// Round trip to the relay. It handles a session's frames in order, so the
/// `Pong` proves every frame sent before the `Ping` has been applied.
/// Returns the other events that arrived meanwhile (e.g. a `Punch`).
async fn relay_sync(relay: &mut RelayClient) -> anyhow::Result<Vec<Event>> {
    relay.ping().await?;
    let pong = async {
        let mut other = Vec::new();
        loop {
            match relay.next_event().await {
                Some(Event::Pong) => return Ok(other),
                Some(ev) => other.push(ev),
                None => bail!("relay connection lost"),
            }
        }
    };
    tokio::time::timeout(SYNC_TIMEOUT, pong)
        .await
        .map_err(|_| anyhow!("relay did not answer"))?
}
/// Local address the OS would use to reach `dest` (no packets sent) — our
/// LAN candidate for peers behind the same NAT.
fn local_ip_toward(dest: SocketAddr) -> Option<IpAddr> {
    let bind: SocketAddr = if dest.is_ipv4() {
        "0.0.0.0:0".parse().ok()?
    } else {
        "[::]:0".parse().ok()?
    };
    let s = std::net::UdpSocket::bind(bind).ok()?;
    s.connect(dest).ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

// ---- keys + TLS ---------------------------------------------------------

/// Tunnel identity key for `role`, derived from `S` under its own HKDF
/// label (independent of the relay-auth and E2E keys).
fn tunnel_key(secret: &PairingSecret, role: Role) -> SigningKey {
    let hk = Hkdf::<Sha256>::new(Some(HKDF_SALT), secret.as_bytes());
    let label: &[u8] = match role {
        Role::Box => b"tunnel-ed25519/box",
        Role::Device => b"tunnel-ed25519/device",
    };
    let mut seed = [0u8; 32];
    hk.expand(label, &mut seed).expect("hkdf");
    SigningKey::from_bytes(&seed)
}

/// Self-signed cert carrying `key`. The cert contents don't matter to the
/// peer — it checks the handshake signature against the pinned key.
fn identity(key: &SigningKey) -> anyhow::Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    // PKCS#8 v1 wrapper for a raw Ed25519 seed (RFC 8410).
    const PKCS8_PREFIX: [u8; 16] = [
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    let mut der = PKCS8_PREFIX.to_vec();
    der.extend_from_slice(&key.to_bytes());
    let pkcs8 = PrivatePkcs8KeyDer::from(der);
    let kp = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(&pkcs8, &rcgen::PKCS_ED25519)?;
    let cert = rcgen::CertificateParams::new(vec![SERVER_NAME.to_string()])?.self_signed(&kp)?;
    Ok((
        CertificateDer::from(cert.der().to_vec()),
        PrivateKeyDer::Pkcs8(pkcs8),
    ))
}

/// Accepts exactly one peer: whoever signs the TLS 1.3 handshake with the
/// Ed25519 key derived from `S` for the other role.
#[derive(Debug)]
struct PinnedPeer {
    key: VerifyingKey,
}

impl PinnedPeer {
    fn check_cert(&self, cert: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        // Belt and braces: the cert must at least carry the pinned key. The
        // real proof is the handshake signature below.
        let key = self.key.as_bytes();
        if cert.windows(key.len()).any(|w| w == key) {
            Ok(())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn check_sig(
        &self,
        message: &[u8],
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let bad = || rustls::Error::InvalidCertificate(rustls::CertificateError::BadSignature);
        if dss.scheme != SignatureScheme::ED25519 {
            return Err(bad());
        }
        let sig = Signature::from_slice(dss.signature()).map_err(|_| bad())?;
        self.key.verify_strict(message, &sig).map_err(|_| bad())?;
        Ok(HandshakeSignatureValid::assertion())
    }
}

impl ServerCertVerifier for PinnedPeer {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check_cert(end_entity)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        _cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.check_sig(message, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

impl ClientCertVerifier for PinnedPeer {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check_cert(end_entity)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        _cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.check_sig(message, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn transport() -> anyhow::Result<Arc<TransportConfig>> {
    let mut t = TransportConfig::default();
    t.keep_alive_interval(Some(KEEPALIVE));
    t.max_idle_timeout(Some(IDLE_TIMEOUT.try_into()?));
    t.max_concurrent_bidi_streams(VarInt::from_u32(256));
    t.max_concurrent_uni_streams(VarInt::from_u32(0));
    Ok(Arc::new(t))
}

fn pinned(key: &SigningKey) -> Arc<PinnedPeer> {
    Arc::new(PinnedPeer {
        key: key.verifying_key(),
    })
}

impl PinnedPeer {
    /// Pin a stored public key (box identity, enrolled device key).
    fn from_public(key: &[u8; 32]) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(
            crate::identity::is_valid_public_key(key),
            "invalid pinned peer key"
        );
        Ok(Arc::new(Self {
            key: VerifyingKey::from_bytes(key)?,
        }))
    }
}

fn certified(key: &SigningKey) -> anyhow::Result<Arc<CertifiedKey>> {
    let (cert, der) = identity(key)?;
    let signer = provider().key_provider.load_private_key(der)?;
    Ok(Arc::new(CertifiedKey::new(vec![cert], signer)))
}

/// A `Link` loop's server cert, by what the client offers: the box
/// identity for `/2`, the `S`-derived cert for an old app's `/1` (which is
/// then closed with a reason it can show).
#[derive(Debug)]
struct AlpnCerts {
    v2: Arc<CertifiedKey>,
    v1: Arc<CertifiedKey>,
}

impl ResolvesServerCert for AlpnCerts {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let v2 = hello
            .alpn()
            .is_some_and(|mut a| a.any(|p| p == TUNNEL_ALPN_V2));
        Some(if v2 { self.v2.clone() } else { self.v1.clone() })
    }
}

fn server_config(cred: &BoxCredential) -> anyhow::Result<quinn::ServerConfig> {
    let builder = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?;
    let mut tls = match cred {
        BoxCredential::Legacy { s, .. } => {
            let (cert, key) = identity(&tunnel_key(s, Role::Box))?;
            let mut t = builder
                .with_client_cert_verifier(pinned(&tunnel_key(s, Role::Device)))
                .with_single_cert(vec![cert], key)?;
            t.alpn_protocols = vec![TUNNEL_ALPN.to_vec()];
            t
        }
        BoxCredential::Link {
            s, identity: id, ..
        } => {
            let certs = AlpnCerts {
                v2: certified(id.signing_key())?,
                v1: certified(&tunnel_key(s, Role::Box))?,
            };
            let mut t = builder
                .with_client_cert_verifier(pinned(&tunnel_key(s, Role::Device)))
                .with_cert_resolver(Arc::new(certs));
            t.alpn_protocols = vec![TUNNEL_ALPN_V2.to_vec(), TUNNEL_ALPN.to_vec()];
            t
        }
        BoxCredential::Enrolled {
            identity: id,
            device_key,
            ..
        } => {
            let (cert, key) = identity(id.signing_key())?;
            let mut t = builder
                .with_client_cert_verifier(PinnedPeer::from_public(device_key)?)
                .with_single_cert(vec![cert], key)?;
            t.alpn_protocols = vec![TUNNEL_ALPN_V2.to_vec()];
            t
        }
    };
    tls.send_tls13_tickets = 0;
    let mut sc = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    sc.transport_config(transport()?);
    Ok(sc)
}

/// A device offers exactly one ALPN, so a box that strips `/2` can't
/// downgrade it: the handshake just fails.
fn client_config(cred: &DeviceCredential) -> anyhow::Result<quinn::ClientConfig> {
    let (me, verifier, alpn) = match cred {
        DeviceCredential::Legacy { link, .. } => (
            tunnel_key(&link.secret, Role::Device),
            pinned(&tunnel_key(&link.secret, Role::Box)),
            TUNNEL_ALPN,
        ),
        DeviceCredential::Link { link, .. } => {
            let k = link
                .box_key
                .ok_or_else(|| anyhow!("pairing link has no box key"))?;
            (
                tunnel_key(&link.secret, Role::Device),
                PinnedPeer::from_public(&k)?,
                TUNNEL_ALPN_V2,
            )
        }
        DeviceCredential::Enrolled(c) => (
            c.device_key().clone(),
            PinnedPeer::from_public(&c.box_key())?,
            TUNNEL_ALPN_V2,
        ),
    };
    let (cert, key) = identity(&me)?;
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![cert], key)?;
    tls.alpn_protocols = vec![alpn.to_vec()];
    let mut cc = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    cc.transport_config(transport()?);
    Ok(cc)
}

fn negotiated_alpn(conn: &Connection) -> Option<Vec<u8>> {
    conn.handshake_data()?
        .downcast::<quinn::crypto::rustls::HandshakeData>()
        .ok()?
        .protocol
}

/// A peer's application close with one of our codes, as its bare reason
/// (`enrolled`, `update-app`, …); anything else as quinn words it.
fn close_text(e: &quinn::ConnectionError) -> String {
    match e {
        quinn::ConnectionError::ApplicationClosed(c)
            if c.error_code == VarInt::from_u32(CLOSE_ENROLLED)
                || c.error_code == VarInt::from_u32(CLOSE_REFUSED) =>
        {
            String::from_utf8_lossy(&c.reason).into_owned()
        }
        e => e.to_string(),
    }
}

fn endpoint(path: PunchedPath, server: Option<quinn::ServerConfig>) -> anyhow::Result<Endpoint> {
    let runtime = Arc::new(quinn::TokioRuntime);
    Ok(match path.socket {
        PathSocket::Direct(sock) => {
            Endpoint::new(EndpointConfig::default(), server, sock.into_std()?, runtime)?
        }
        PathSocket::Relayed(r) => Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            server,
            r.into_socket(path.peer),
            runtime,
        )?,
    })
}

fn rtt_ms(c: &Connection) -> u32 {
    c.rtt().as_millis().min(u32::MAX as u128) as u32
}

// ---- box ----------------------------------------------------------------

/// What a box connection may do, from the loop's credential and the
/// negotiated ALPN (see the table in the module docs).
#[derive(Clone, Copy, Debug)]
enum Policy {
    /// Forward TCP + ping (+ enrollment per the gate).
    Full,
    /// Ping + enrollment only, for at most [`ENROLL_ONLY_MAX`].
    EnrollOnly,
    /// Close at once with this reason; `reuse`: report a link reuse.
    Close {
        reason: &'static str,
        reuse: Option<RefuseReason>,
    },
}

fn session_policy(
    cred: &BoxCredential,
    alpn: Option<&[u8]>,
    handler: bool,
) -> (Policy, Option<enroll::EnrollGate>) {
    let v1 = alpn == Some(TUNNEL_ALPN);
    let v2 = alpn == Some(TUNNEL_ALPN_V2);
    match cred {
        BoxCredential::Legacy { identity, .. } if v1 => (
            Policy::Full,
            (identity.is_some() && handler).then_some(enroll::EnrollGate::Upgrade),
        ),
        BoxCredential::Link { mode, .. } if v2 => {
            let refuse = match mode {
                LinkMode::Enroll => None,
                LinkMode::Refuse(r) => Some(*r),
            };
            (
                Policy::EnrollOnly,
                Some(enroll::EnrollGate::Link { refuse }),
            )
        }
        BoxCredential::Link { mode, .. } if v1 => {
            let close = match mode {
                LinkMode::Enroll => Policy::Close {
                    reason: "update-app",
                    reuse: None,
                },
                LinkMode::Refuse(r) => Policy::Close {
                    reason: r.close_reason(),
                    reuse: Some(*r),
                },
            };
            (close, None)
        }
        BoxCredential::Enrolled { .. } if v2 => (Policy::Full, None),
        _ => (
            Policy::Close {
                reason: "protocol",
                reuse: None,
            },
            None,
        ),
    }
}

fn legacy_box(secret: &PairingSecret) -> BoxCredential {
    BoxCredential::Legacy {
        s: secret.clone(),
        identity: None,
    }
}

/// Box side: accept QUIC from the paired device on the punched path and
/// forward every stream to `target` (box-side config only — the device
/// cannot choose it). Only `path.peer` may connect (see [`accept_device`]).
/// Returns `Ok` when an established connection ends, `Err` if no
/// authenticated device connects within 15 s. A legacy pairing; see
/// [`serve_box_with`].
pub async fn serve_box(
    path: PunchedPath,
    secret: &PairingSecret,
    target: SocketAddr,
    on_event: impl Fn(TunnelEvent) + Send + Sync + 'static,
) -> anyhow::Result<()> {
    serve_box_with(path, &legacy_box(secret), target, None, on_event).await
}

/// [`serve_box`] for any credential. `enroll` answers enrollment
/// requests (required for `Link` loops and legacy upgrades) and hears
/// about activations of `Enrolled` loops. Establish `path` with
/// [`BoxCredential::relay_secret`].
pub async fn serve_box_with(
    path: PunchedPath,
    cred: &BoxCredential,
    target: SocketAddr,
    enroll: Option<Arc<dyn EnrollHandler>>,
    on_event: impl Fn(TunnelEvent) + Send + Sync + 'static,
) -> anyhow::Result<()> {
    serve_box_inner(path, cred, target, None, enroll, on_event).await
}

/// How [`serve_box_rejoining`] stays at the relay while serving: the same
/// relay and options the served path was established with.
#[derive(Clone)]
pub struct BoxRejoin {
    pub cfg: ClientConfig,
    /// Used as is, except the standby binds an ephemeral port and
    /// advertises nothing: `bind_port` (and so every [`Advertise`] address
    /// pointing at it) is held by the served path.
    pub opts: EstablishOptions,
}

/// [`serve_box`] that stays reachable for the device's next round while it
/// serves a direct path. A device whose network changed (Wi-Fi ↔ cellular)
/// abandons the old path, and its CONNECTION_CLOSE leaves from the new
/// address — which the box's NAT typically drops — so a box that only
/// re-registers once the connection ends would keep the device waiting for
/// the ~15 s ping timeout. Instead a standby [`establish_with`] keeps a
/// relay session for this pairing open the whole time (relay failures are
/// retried with backoff, without touching the served connection). When it
/// punches a new path, the device's authenticated handshake on it
/// (accepted only from the new punched peer) replaces the served
/// connection: the old one is closed and `Connected` is reported again. A
/// round that fails leaves the served connection alone; newest round wins.
///
/// A relayed path carries its data over the relay session itself (a second
/// box session would replace it at the relay), so no standby runs while
/// one is served. Returns like [`serve_box`]; when the served connection
/// dies while a standby round is mid-handshake, that round is awaited
/// first. A legacy pairing; see [`serve_box_rejoining_with`].
pub async fn serve_box_rejoining(
    path: PunchedPath,
    secret: &PairingSecret,
    target: SocketAddr,
    rejoin: &BoxRejoin,
    on_event: impl Fn(TunnelEvent) + Send + Sync + 'static,
) -> anyhow::Result<()> {
    serve_box_rejoining_with(path, &legacy_box(secret), target, rejoin, None, on_event).await
}

/// [`serve_box_rejoining`] for any credential (see [`serve_box_with`]).
pub async fn serve_box_rejoining_with(
    path: PunchedPath,
    cred: &BoxCredential,
    target: SocketAddr,
    rejoin: &BoxRejoin,
    enroll: Option<Arc<dyn EnrollHandler>>,
    on_event: impl Fn(TunnelEvent) + Send + Sync + 'static,
) -> anyhow::Result<()> {
    serve_box_inner(path, cred, target, Some(rejoin), enroll, on_event).await
}

/// First standby retry delay after a relay failure, doubling up to
/// [`STANDBY_BACKOFF_MAX`]; reset once a standby session lasted
/// [`MAINTAIN_EVERY`].
const STANDBY_BACKOFF_MIN: Duration = Duration::from_secs(1);
const STANDBY_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// One authenticated device connection on its own endpoint. Dropping it
/// closes both, so a box serve future that is aborted (device revoked,
/// remote access disabled) tells the device at once and frees the socket,
/// instead of leaving the endpoint driver alive behind it.
struct Served {
    ep: Endpoint,
    conn: Connection,
    path_rx: watch::Receiver<PathKind>,
    direct: bool,
    policy: Policy,
    gate: Option<enroll::EnrollGate>,
}

impl Served {
    fn connected(&mut self) -> TunnelEvent {
        TunnelEvent::Connected {
            peer: self.conn.remote_address(),
            rtt_ms: rtt_ms(&self.conn),
            path: *self.path_rx.borrow_and_update(),
        }
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        // Directly on the connection (see the replacement in
        // `serve_box_inner`); keep an earlier, more specific reason.
        if self.conn.close_reason().is_none() {
            self.conn.close(VarInt::from_u32(0), b"stopped");
        }
        self.ep.close(VarInt::from_u32(0), b"stopped");
    }
}

/// Endpoint on `path` plus the device's authenticated connection from
/// `path.peer` (see [`accept_device`]).
async fn accept_path(
    path: PunchedPath,
    cred: &BoxCredential,
    handler: bool,
) -> anyhow::Result<Served> {
    tracing::debug!(peer = %path.peer, path = %path.kind(), cred = cred.kind(), "tunnel: awaiting device QUIC");
    let path_rx = path.path_watch();
    let direct = matches!(path.socket, PathSocket::Direct(_));
    let peer = path.peer;
    let ep = endpoint(path, Some(server_config(cred)?))?;
    let conn = accept_device(&ep, peer).await?;
    let (policy, gate) = session_policy(cred, negotiated_alpn(&conn).as_deref(), handler);
    Ok(Served {
        ep,
        conn,
        path_rx,
        direct,
        policy,
        gate,
    })
}

/// Standby registration for [`serve_box_rejoining`]: wait at the relay for
/// the device's next round and accept its handshake on the new path.
/// `accepting` is true while a punched path awaits that handshake. Never
/// fails: relay errors are retried with backoff, a failed round starts the
/// next.
async fn standby(
    rejoin: &BoxRejoin,
    cred: &BoxCredential,
    handler: bool,
    accepting: &watch::Sender<bool>,
) -> Served {
    let opts = EstablishOptions {
        bind_port: None,
        advertise: Vec::new(),
        ..rejoin.opts.clone()
    };
    let secret = cred.relay_secret();
    let mut backoff = STANDBY_BACKOFF_MIN;
    loop {
        let started = tokio::time::Instant::now();
        match establish_with(&rejoin.cfg, &secret, Role::Box, &opts).await {
            Ok(path) => {
                accepting.send_replace(true);
                let r = accept_path(path, cred, handler).await;
                accepting.send_replace(false);
                match r {
                    Ok(s) => return s,
                    Err(e) => tracing::debug!("tunnel: standby round failed: {e:#}"),
                }
            }
            Err(e) => {
                tracing::debug!("tunnel: standby relay session failed, retrying: {e:#}");
                if started.elapsed() >= MAINTAIN_EVERY {
                    backoff = STANDBY_BACKOFF_MIN;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(STANDBY_BACKOFF_MAX);
            }
        }
    }
}

/// What every stream of one box connection needs.
struct StreamCtx {
    target: SocketAddr,
    /// [`Policy::Full`]: forward TCP streams.
    forward: bool,
    enroll: Option<enroll::BoxEnroll>,
}

async fn serve_box_inner(
    path: PunchedPath,
    cred: &BoxCredential,
    target: SocketAddr,
    rejoin: Option<&BoxRejoin>,
    handler: Option<Arc<dyn EnrollHandler>>,
    on_event: impl Fn(TunnelEvent) + Send + Sync + 'static,
) -> anyhow::Result<()> {
    let mut cur = match accept_path(path, cred, handler.is_some()).await {
        Ok(s) => s,
        Err(e) => {
            on_event(TunnelEvent::Error(format!("{e:#}")));
            return Err(e);
        }
    };
    let (accepting_tx, mut accepting) = watch::channel(false);
    loop {
        let from = cur.conn.remote_address();
        if let Policy::Close { reason, reuse } = cur.policy {
            tracing::info!(%from, reason, cred = cred.kind(), "tunnel: closing a connection this pairing doesn't serve");
            if let (Some(r), Some(h)) = (reuse, &handler) {
                h.link_reuse(from, r).await;
            }
            cur.conn
                .close(VarInt::from_u32(CLOSE_REFUSED), reason.as_bytes());
            on_event(TunnelEvent::Disconnected {
                reason: reason.to_string(),
            });
            // Let the close reach the device before the endpoint goes.
            let _ = tokio::time::timeout(Duration::from_secs(1), cur.ep.wait_idle()).await;
            return Ok(());
        }
        if let (BoxCredential::Enrolled { device_key, .. }, Some(h)) = (cred, &handler) {
            h.activated(*device_key, from).await;
        }
        on_event(cur.connected());
        let ctx = Arc::new(StreamCtx {
            target,
            forward: matches!(cur.policy, Policy::Full),
            enroll: match (cur.gate, cred.identity()) {
                (Some(gate), Some(identity)) => Some(enroll::BoxEnroll {
                    conn: cur.conn.clone(),
                    gate,
                    identity: identity.clone(),
                    handler: handler.clone(),
                }),
                _ => None,
            },
        });
        let enroll_only = matches!(cur.policy, Policy::EnrollOnly);
        let cap = tokio::time::sleep(ENROLL_ONLY_MAX);
        tokio::pin!(cap);
        let direct = cur.direct;
        let next = async {
            match rejoin {
                Some(r) if direct => standby(r, cred, handler.is_some(), &accepting_tx).await,
                _ => std::future::pending().await,
            }
        };
        tokio::pin!(next);
        // A ping stream that goes quiet reports here (see `box_pong`). One
        // channel per connection, so a replaced connection's late timeout
        // can't end its successor.
        let (dead_tx, mut dead_rx) = mpsc::channel::<()>(1);
        // This connection's stream tasks: owned here, so they end with it
        // (and with this future, when it is aborted).
        let mut streams = tokio::task::JoinSet::new();
        let ended = loop {
            tokio::select! {
                Ok(()) = cur.path_rx.changed() => {
                    on_event(TunnelEvent::PathChanged { path: *cur.path_rx.borrow_and_update() });
                }
                Some(_) = streams.join_next(), if !streams.is_empty() => {}
                s = cur.conn.accept_bi() => match s {
                    Ok((send, recv)) => {
                        streams.spawn(box_stream(send, recv, ctx.clone(), dead_tx.clone()));
                    }
                    Err(e) => break Err(close_text(&e)),
                },
                Some(()) = dead_rx.recv() => {
                    break Err(format!("device stopped pinging ({PING_MISSES} missed)"));
                }
                _ = &mut cap, if enroll_only => {
                    cur.conn.close(VarInt::from_u32(CLOSE_REFUSED), b"enrollment window closed");
                    break Err("enrollment window closed".to_string());
                }
                // One device per path: refuse anything else on it. A new
                // path arrives through the standby round.
                Some(inc) = cur.ep.accept() => inc.refuse(),
                new = &mut next => break Ok(new),
            }
        };
        let new = match ended {
            Ok(new) => new,
            Err(reason) => {
                on_event(TunnelEvent::Disconnected { reason });
                cur.ep.close(VarInt::from_u32(0), b"");
                // The device may be mid-handshake on its new path already.
                if !*accepting.borrow_and_update() {
                    return Ok(());
                }
                tokio::select! {
                    biased;
                    new = &mut next => new,
                    _ = accepting.wait_for(|a| !*a) => return Ok(()),
                }
            }
        };
        tracing::info!(
            old = %cur.conn.remote_address(),
            new = %new.conn.remote_address(),
            "tunnel: device reconnected on a new path, replacing the old connection"
        );
        // Directly on the connection: the endpoint's close is queued to the
        // connection driver, and dropping the handle first would close it
        // without a reason.
        cur.conn.close(VarInt::from_u32(0), b"replaced");
        cur.ep.close(VarInt::from_u32(0), b"replaced");
        cur = new;
    }
}

/// The first connection from `peer` that completes the pinned handshake.
/// Initials from any other address are refused (on a relayed path every
/// packet appears to come from `peer`), and handshakes run concurrently,
/// so a stranger that spoofs `peer` and stalls, or fails authentication,
/// can't keep the real device out. Gives up after [`HANDSHAKE_TIMEOUT`],
/// reporting the last handshake error if there was one.
async fn accept_device(ep: &Endpoint, peer: SocketAddr) -> anyhow::Result<Connection> {
    let canonical = |a: SocketAddr| SocketAddr::new(a.ip().to_canonical(), a.port());
    let peer = canonical(peer);
    let deadline = tokio::time::sleep(HANDSHAKE_TIMEOUT);
    tokio::pin!(deadline);
    let mut handshakes = tokio::task::JoinSet::new();
    let mut last_err: Option<anyhow::Error> = None;
    loop {
        tokio::select! {
            _ = &mut deadline => {
                return Err(last_err.unwrap_or_else(|| anyhow!("device did not connect")));
            }
            inc = ep.accept() => {
                let inc = inc.ok_or_else(|| anyhow!("endpoint closed"))?;
                let from = inc.remote_address();
                if canonical(from) != peer || handshakes.len() >= MAX_HANDSHAKES {
                    tracing::debug!(%from, "tunnel: refusing QUIC (not the punched peer, or too many handshakes)");
                    inc.refuse();
                    continue;
                }
                tracing::debug!(%from, "tunnel: device QUIC incoming");
                handshakes.spawn(async move { inc.await });
            }
            Some(done) = handshakes.join_next(), if !handshakes.is_empty() => match done {
                Ok(Ok(conn)) => return Ok(conn),
                Ok(Err(e)) => {
                    tracing::debug!("tunnel: device handshake failed: {e}");
                    last_err = Some(anyhow::Error::new(e).context("device handshake failed"));
                }
                Err(_) => {}
            },
        }
    }
}

async fn box_stream(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    ctx: Arc<StreamCtx>,
    dead: mpsc::Sender<()>,
) {
    let mut ty = [0u8; 1];
    let ok = matches!(
        tokio::time::timeout(HEADER_TIMEOUT, recv.read_exact(&mut ty)).await,
        Ok(Ok(()))
    );
    if ok && ty[0] == STREAM_PING {
        return box_pong(send, recv, dead).await;
    }
    if ok
        && ty[0] == STREAM_ENROLL
        && let Some(e) = &ctx.enroll
    {
        return enroll::serve(send, recv, e).await;
    }
    if !ok || ty[0] != STREAM_TCP || !ctx.forward {
        let _ = send.reset(VarInt::from_u32(1));
        let _ = recv.stop(VarInt::from_u32(1));
        return;
    }
    match TcpStream::connect(ctx.target).await {
        Ok(tcp) => pipe(send, recv, tcp).await,
        Err(_) => {
            let _ = send.reset(VarInt::from_u32(2));
            let _ = recv.stop(VarInt::from_u32(2));
        }
    }
}

/// Box half of the liveness check: echo every ping byte. If an established
/// ping stream goes quiet for [`PING_MISSES`] intervals the device is gone;
/// tell [`serve_box`]. A device that never opens one (older build) is only
/// caught by the QUIC idle timeout.
async fn box_pong(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    dead: mpsc::Sender<()>,
) {
    let mut buf = [0u8; 16];
    loop {
        match tokio::time::timeout(PING_EVERY * PING_MISSES, recv.read(&mut buf)).await {
            Ok(Ok(Some(n))) => {
                if send.write_all(&buf[..n]).await.is_err() {
                    return;
                }
            }
            Ok(_) => return,
            Err(_) => {
                let _ = dead.try_send(());
                return;
            }
        }
    }
}

// ---- device -------------------------------------------------------------

fn legacy_device(secret: &PairingSecret) -> DeviceCredential {
    DeviceCredential::Legacy {
        link: PairingLink::new(secret.clone(), DEFAULT_RELAY),
        upgrade_key: None,
    }
}

/// Device side: connect QUIC to the box over the punched path and expose
/// `listen`; each accepted TCP connection becomes one stream. Returns `Ok`
/// when an established tunnel ends (connections queued on `listen` in the
/// meantime are served by the next call), `Err` if the handshake fails.
/// [`run_device`] wraps this in a reconnect loop with an accept filter. A
/// legacy pairing; [`run_device`] handles every [`DeviceCredential`].
pub async fn connect_device(
    path: PunchedPath,
    secret: &PairingSecret,
    listen: &TcpListener,
    on_event: impl Fn(TunnelEvent) + Send + Sync + 'static,
) -> anyhow::Result<()> {
    let never = CancellationToken::new();
    device_session(
        path,
        &legacy_device(secret),
        listen,
        None,
        &never,
        None,
        None,
        on_event,
    )
    .await
}

/// A legacy-upgrade attempt to run alongside a legacy tunnel.
struct Upgrade<'a> {
    key: &'a SigningKey,
    name: &'a str,
    on_enrolled: Option<&'a OnEnrolled>,
    /// Gets the attempt's result once (not called if the tunnel ends first).
    report: &'a (dyn Fn(anyhow::Result<enroll::Outcome>) + Send + Sync),
}

/// [`connect_device`] for `cred` (`Legacy` or `Enrolled`), plus an
/// optional [`AcceptFilter`] run on every accepted connection, a `cancel`
/// token that ends the tunnel (and every stream on it) with
/// `Disconnected { reason: "stopped" }`, an optional [`DeviceKick`]: a
/// network change closes the connection at once (`Disconnected { reason:
/// "network changed" }`) instead of waiting for the pings to time out, and
/// an optional legacy [`Upgrade`].
#[allow(clippy::too_many_arguments)]
async fn device_session(
    path: PunchedPath,
    cred: &DeviceCredential,
    listen: &TcpListener,
    filter: Option<&AcceptFilter>,
    cancel: &CancellationToken,
    kick: Option<&DeviceKick>,
    upgrade: Option<Upgrade<'_>>,
    on_event: impl Fn(TunnelEvent) + Send + Sync + 'static,
) -> anyhow::Result<()> {
    let kicked = || async move {
        match kick {
            Some(k) => k.notified().await,
            None => std::future::pending().await,
        }
    };
    let mut path_rx = path.path_watch();
    let handshake = tokio::select! {
        r = connect_raw_with(path, cred) => r,
        _ = cancel.cancelled() => return Ok(()),
        _ = kicked() => return Ok(()),
    };
    let (ep, conn) = match handshake {
        Ok(v) => v,
        Err(e) => {
            on_event(TunnelEvent::Error(format!("{e:#}")));
            return Err(e);
        }
    };
    on_event(TunnelEvent::Connected {
        peer: conn.remote_address(),
        rtt_ms: rtt_ms(&conn),
        path: *path_rx.borrow_and_update(),
    });
    let alive = device_liveness(conn.clone());
    tokio::pin!(alive);
    let mut upgrading = upgrade.is_some();
    let upgrade_run = async {
        match &upgrade {
            Some(u) => {
                enroll::device_enroll(
                    &conn,
                    EnrollMode::LegacyUpgrade,
                    u.key,
                    u.name,
                    None,
                    cred.relay_host(),
                    u.on_enrolled,
                )
                .await
            }
            None => std::future::pending().await,
        }
    };
    tokio::pin!(upgrade_run);
    let reason = loop {
        tokio::select! {
            e = conn.closed() => break close_text(&e),
            r = &mut alive => break r,
            Ok(()) = path_rx.changed() => {
                on_event(TunnelEvent::PathChanged { path: *path_rx.borrow_and_update() });
            }
            r = &mut upgrade_run, if upgrading => {
                upgrading = false;
                if let Some(u) = &upgrade {
                    (u.report)(r);
                }
            }
            _ = cancel.cancelled() => break "stopped".to_string(),
            _ = kicked() => break "network changed".to_string(),
            a = listen.accept() => match a {
                Ok((tcp, _)) => {
                    let conn = conn.clone();
                    let filter = filter.cloned();
                    tokio::spawn(async move {
                        let Admitted { head, tcp } = match filter {
                            Some(f) => match f(tcp).await {
                                Some(a) => a,
                                None => return,
                            },
                            None => Admitted::new(tcp),
                        };
                        let Ok((mut send, recv)) = conn.open_bi().await else { return };
                        if send.write_all(&[STREAM_TCP]).await.is_ok()
                            && send.write_all(&head).await.is_ok()
                        {
                            pipe(send, recv, tcp).await;
                        }
                    });
                }
                Err(e) => {
                    on_event(TunnelEvent::Error(format!("local accept: {e}")));
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
        }
    };
    on_event(TunnelEvent::Disconnected { reason });
    ep.close(VarInt::from_u32(0), b"");
    Ok(())
}

/// One enrollment-only round for a v2 link (`cred` must be
/// [`DeviceCredential::Link`]): handshake pinned to the link's box key,
/// enroll, wait for the box to close with `enrolled`. No `Connected`; the
/// local listener isn't served. `None`: cancelled or kicked.
async fn enroll_session(
    path: PunchedPath,
    cred: &DeviceCredential,
    name: &str,
    on_enrolled: Option<&OnEnrolled>,
    cancel: &CancellationToken,
    kick: &DeviceKick,
) -> anyhow::Result<Option<enroll::Outcome>> {
    let DeviceCredential::Link { link, device_key } = cred else {
        bail!("enroll_session needs a v2 pairing link");
    };
    let work = async {
        let (ep, conn) = connect_raw_with(path, cred).await?;
        let out = enroll::device_enroll(
            &conn,
            EnrollMode::Link,
            device_key,
            name,
            link.box_key,
            &link.relay,
            on_enrolled,
        )
        .await;
        if matches!(out, Ok(enroll::Outcome::Enrolled(_))) {
            // The box closes once it has the ack.
            let _ = tokio::time::timeout(HEADER_TIMEOUT, conn.closed()).await;
        }
        conn.close(VarInt::from_u32(0), b"");
        ep.close(VarInt::from_u32(0), b"");
        out
    };
    tokio::select! {
        r = work => r.map(Some),
        _ = cancel.cancelled() => Ok(None),
        _ = kick.notified() => Ok(None),
    }
}

/// Device half of the liveness check: one [`STREAM_PING`] stream, a byte
/// every [`PING_EVERY`], the box echoes it. Resolves once no pong arrived
/// for [`PING_MISSES`] intervals. A box that predates pings resets the
/// stream; then this never resolves and the QUIC idle timeout applies.
async fn device_liveness(conn: Connection) -> String {
    let fallback = std::future::pending::<String>;
    let Ok((mut send, mut recv)) = conn.open_bi().await else {
        return fallback().await;
    };
    if send.write_all(&[STREAM_PING]).await.is_err() {
        return fallback().await;
    }
    let silence = PING_EVERY * PING_MISSES;
    let mut tick = tokio::time::interval(PING_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let deadline = tokio::time::sleep(silence);
    tokio::pin!(deadline);
    let mut buf = [0u8; 16];
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if send.write_all(&[0]).await.is_err() {
                    return fallback().await;
                }
            }
            _ = &mut deadline => {
                return format!("box stopped answering pings ({PING_MISSES} missed)");
            }
            r = recv.read(&mut buf) => match r {
                Ok(Some(_)) => deadline.as_mut().reset(tokio::time::Instant::now() + silence),
                _ => return fallback().await,
            },
        }
    }
}

/// The device's authenticated QUIC connection, without the TCP plumbing
/// (legacy pairing). Test hook; keep the returned endpoint alive with the
/// connection.
#[doc(hidden)]
pub async fn connect_raw(
    path: PunchedPath,
    secret: &PairingSecret,
) -> anyhow::Result<(Endpoint, Connection)> {
    connect_raw_with(path, &legacy_device(secret)).await
}

/// [`connect_raw`] for any credential. Test hook.
#[doc(hidden)]
pub async fn connect_raw_with(
    path: PunchedPath,
    cred: &DeviceCredential,
) -> anyhow::Result<(Endpoint, Connection)> {
    let peer = path.peer;
    tracing::debug!(%peer, cred = cred.kind(), "tunnel: QUIC connect");
    let ep = endpoint(path, None)?;
    let connecting = ep.connect_with(client_config(cred)?, peer, SERVER_NAME)?;
    let conn = tokio::time::timeout(HANDSHAKE_TIMEOUT, connecting)
        .await
        .map_err(|_| anyhow!("box did not answer"))?
        .context("box handshake failed")?;
    Ok((ep, conn))
}
// ---- plumbing -----------------------------------------------------------

/// Copy both directions with half-close; on an error in either direction,
/// reset the stream so neither side hangs.
async fn pipe(mut send: quinn::SendStream, mut recv: quinn::RecvStream, tcp: TcpStream) {
    let _ = tcp.set_nodelay(true);
    let (mut tr, mut tw) = tcp.into_split();
    let failed = {
        let up = async {
            let r = tokio::io::copy(&mut recv, &mut tw).await;
            let _ = tw.shutdown().await;
            r
        };
        let down = async {
            let r = tokio::io::copy(&mut tr, &mut send).await;
            if r.is_ok() {
                let _ = send.finish();
            }
            r
        };
        tokio::pin!(up, down);
        let (mut up_done, mut down_done) = (false, false);
        loop {
            tokio::select! {
                r = &mut up, if !up_done => {
                    up_done = true;
                    if r.is_err() { break true; }
                }
                r = &mut down, if !down_done => {
                    down_done = true;
                    if r.is_err() { break true; }
                }
            }
            if up_done && down_done {
                break false;
            }
        }
    };
    if failed {
        let _ = send.reset(VarInt::from_u32(3));
        let _ = recv.stop(VarInt::from_u32(3));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnel_keys_are_separate_from_relay_keys() {
        let s = PairingSecret::from_bytes([3; 32]);
        let d = s.derive();
        let b = tunnel_key(&s, Role::Box).verifying_key().to_bytes();
        let v = tunnel_key(&s, Role::Device).verifying_key().to_bytes();
        assert_ne!(b, v);
        assert_ne!(b, d.public_key());
        assert_ne!(v, d.public_key());
        identity(&tunnel_key(&s, Role::Box)).unwrap();
    }
}
