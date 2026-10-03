//! Direct box↔device data path (`tunnel` feature).
//!
//! After rendezvous + hole punch ([`establish`]) the two peers share one UDP
//! 5-tuple. QUIC (quinn) runs over that very socket — reusing it keeps the
//! NAT mapping the punch opened. The box is the QUIC server, the device the
//! client; TLS 1.3 inside QUIC is mutually authenticated against Ed25519
//! keys derived from the pairing secret `S` (labels distinct from the relay
//! auth and E2E keys), with no CA involved. ALPN [`TUNNEL_ALPN`].
//!
//! Every bidirectional stream is one forwarded TCP connection: a 1-byte
//! type header ([`STREAM_TCP`]) followed by raw bytes. The box pipes it to
//! its configured local target; the device has no way to choose that
//! target. HTTP, WebSocket and uploads all pass through untouched.
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
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use hkdf::Hkdf;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Connection, Endpoint, EndpointConfig, TransportConfig, VarInt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use sha2::Sha256;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;

use crate::client::{ClientConfig, Event, RelayClient};
use crate::keys::{PairingSecret, SECRET_LEN};
use crate::proto::Role;

mod device;
pub use device::{
    AcceptFilter, CookieGate, DeviceEvent, DeviceOptions, ListenAddr, bind_listener, run_device,
};
pub use quinn;
/// Stops [`run_device`]; re-exported so callers need no `tokio-util` dep.
pub use tokio_util::sync::CancellationToken;

/// ALPN of the box↔device QUIC connection.
pub const TUNNEL_ALPN: &[u8] = b"peckboard-tunnel/1";
/// Stream type: forward to the box's configured local TCP target.
pub const STREAM_TCP: u8 = 0x01;
/// Stream type: liveness ping. The device writes one byte every 5 s, the
/// box echoes it; either side drops the tunnel after 3 silent intervals.
/// Peers that predate it reset the stream and fall back to the idle timeout.
pub const STREAM_PING: u8 = 0x02;
/// Default rendezvous server.
pub const DEFAULT_RELAY: &str = "relay.peckboard.com";

const LINK_PREFIX: &str = "peckboard://pair/";
const HKDF_SALT: &[u8] = b"peckboard-relay/v1";
const SERVER_NAME: &str = "peckboard-tunnel";
const KEEPALIVE: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// App-level liveness ([`STREAM_PING`]): a dead peer is noticed after
/// `PING_EVERY * PING_MISSES` instead of [`IDLE_TIMEOUT`].
const PING_EVERY: Duration = Duration::from_secs(5);
const PING_MISSES: u32 = 3;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
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
}

/// The punched path, ready for [`serve_box`] / [`connect_device`].
pub struct PunchedPath {
    /// The exact socket the punch used (same local port ⇒ same NAT mapping).
    pub socket: UdpSocket,
    /// The peer's address as seen from `socket`.
    pub peer: SocketAddr,
    pub role: Role,
}

#[derive(Clone, Debug)]
pub enum TunnelEvent {
    Connected { peer: SocketAddr, rtt_ms: u32 },
    Disconnected { reason: String },
    Error(String),
}

// ---- pairing link -------------------------------------------------------

/// `peckboard://pair/<base64url(S)>?relay=<host[:port]>`.
#[derive(Clone)]
pub struct PairingLink {
    pub secret: PairingSecret,
    pub relay: String,
}

impl fmt::Debug for PairingLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingLink")
            .field("relay", &self.relay)
            .finish_non_exhaustive()
    }
}

impl PairingLink {
    pub fn new(secret: PairingSecret, relay: &str) -> Self {
        Self {
            secret,
            relay: relay.to_string(),
        }
    }

    pub fn to_uri(&self) -> String {
        format!(
            "{LINK_PREFIX}{}?relay={}",
            URL_SAFE_NO_PAD.encode(self.secret.as_bytes()),
            self.relay
        )
    }

    pub fn parse(link: &str) -> anyhow::Result<Self> {
        let rest = link
            .trim()
            .strip_prefix(LINK_PREFIX)
            .ok_or_else(|| anyhow!("not a peckboard://pair/ link"))?;
        let (b64, query) = rest.split_once('?').unwrap_or((rest, ""));
        let bytes = URL_SAFE_NO_PAD
            .decode(b64.trim_end_matches('/').trim_end_matches('='))
            .map_err(|_| anyhow!("pairing link: secret is not base64url"))?;
        let secret: [u8; SECRET_LEN] = bytes
            .try_into()
            .map_err(|_| anyhow!("pairing link: secret must be {SECRET_LEN} bytes"))?;
        let mut relay = DEFAULT_RELAY.to_string();
        for kv in query.split('&').filter(|s| !s.is_empty()) {
            if let Some(v) = kv.strip_prefix("relay=")
                && !v.is_empty()
            {
                relay = v.to_string();
            }
        }
        Ok(Self {
            secret: PairingSecret::from_bytes(secret),
            relay,
        })
    }
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
}

impl fmt::Debug for EstablishOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EstablishOptions")
            .field("bind_port", &self.bind_port)
            .field("advertise", &self.advertise)
            .field("public_ip_hint", &self.public_ip_hint)
            .finish_non_exhaustive()
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
/// the box isn't seen within 30 s. Either side returns
/// [`TunnelError::PunchFailed`] when punch rounds keep failing. The relay
/// session is closed on return — a reconnect is a fresh `establish`.
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
    let mut relay = RelayClient::connect(cfg, secret, role)
        .await
        .context("connect to relay")?;
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
    let lan = local_ip_toward(cfg.relay).map(|ip| SocketAddr::new(ip, port));
    let (mut cands, pending) = candidates(lan, &opts.advertise, opts.public_ip_hint, v4);
    if !cands.is_empty() {
        relay.set_candidates(&cands).await?;
        // The STUN Binding below (UDP) is what lets the relay coordinate the
        // punch; it can overtake the candidates frame (TLS). Then the peer
        // is told to punch without our LAN address and, where the LAN path
        // is the only one that works back to us, the punch is one-sided:
        // the peer "succeeds", we time out, and the reconnect costs ~15 s.
        relay_sync(&mut relay).await?;
    }
    let public = relay.stun_binding(&sock).await.context("relay STUN")?;
    let (resolved, _) = candidates(lan, &opts.advertise, Some(public.ip()), v4);
    let mut backlog = std::collections::VecDeque::new();
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
        });
    }

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
                Some(Event::Punch(p)) => match relay.punch(&sock, &p, PUNCH_TIMEOUT).await {
                    Ok(peer) => {
                        return Ok(PunchedPath { socket: sock, peer, role });
                    }
                    Err(_) => {
                        failures += 1;
                        if failures >= MAX_PUNCH_FAILURES {
                            return Err(TunnelError::PunchFailed { rounds: failures }.into());
                        }
                        deadline = Some(tokio::time::Instant::now() + RETRY_WAIT);
                        let _ = relay.request_punch().await;
                    }
                },
                Some(_) => {}
            }
        }
    }
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

fn peer_verifier(secret: &PairingSecret, me: Role) -> Arc<PinnedPeer> {
    Arc::new(PinnedPeer {
        key: tunnel_key(secret, me.other()).verifying_key(),
    })
}

fn server_config(secret: &PairingSecret) -> anyhow::Result<quinn::ServerConfig> {
    let (cert, key) = identity(&tunnel_key(secret, Role::Box))?;
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(peer_verifier(secret, Role::Box))
        .with_single_cert(vec![cert], key)?;
    tls.alpn_protocols = vec![TUNNEL_ALPN.to_vec()];
    tls.send_tls13_tickets = 0;
    let mut sc = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    sc.transport_config(transport()?);
    Ok(sc)
}

fn client_config(secret: &PairingSecret) -> anyhow::Result<quinn::ClientConfig> {
    let (cert, key) = identity(&tunnel_key(secret, Role::Device))?;
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(peer_verifier(secret, Role::Device))
        .with_client_auth_cert(vec![cert], key)?;
    tls.alpn_protocols = vec![TUNNEL_ALPN.to_vec()];
    let mut cc = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    cc.transport_config(transport()?);
    Ok(cc)
}

fn endpoint(path: PunchedPath, server: Option<quinn::ServerConfig>) -> anyhow::Result<Endpoint> {
    let sock = path.socket.into_std()?;
    Ok(Endpoint::new(
        EndpointConfig::default(),
        server,
        sock,
        Arc::new(quinn::TokioRuntime),
    )?)
}

fn rtt_ms(c: &Connection) -> u32 {
    c.rtt().as_millis().min(u32::MAX as u128) as u32
}

// ---- box ----------------------------------------------------------------

/// Box side: accept QUIC from the paired device on the punched path and
/// forward every stream to `target` (box-side config only — the device
/// cannot choose it). Returns `Ok` when an established connection ends,
/// `Err` if no authenticated device connects within 15 s.
pub async fn serve_box(
    path: PunchedPath,
    secret: &PairingSecret,
    target: SocketAddr,
    on_event: impl Fn(TunnelEvent) + Send + Sync + 'static,
) -> anyhow::Result<()> {
    tracing::debug!(peer = %path.peer, "tunnel: awaiting device QUIC");
    let ep = endpoint(path, Some(server_config(secret)?))?;
    let conn = match tokio::time::timeout(HANDSHAKE_TIMEOUT, accept_one(&ep)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            on_event(TunnelEvent::Error(format!("{e:#}")));
            return Err(e);
        }
        Err(_) => {
            let e = anyhow!("device did not connect");
            on_event(TunnelEvent::Error(e.to_string()));
            return Err(e);
        }
    };
    on_event(TunnelEvent::Connected {
        peer: conn.remote_address(),
        rtt_ms: rtt_ms(&conn),
    });
    // A ping stream that goes quiet reports here (see `box_pong`).
    let (dead_tx, mut dead_rx) = mpsc::channel::<()>(1);
    let reason = loop {
        tokio::select! {
            s = conn.accept_bi() => match s {
                Ok((send, recv)) => {
                    tokio::spawn(box_stream(send, recv, target, dead_tx.clone()));
                }
                Err(e) => break e.to_string(),
            },
            Some(()) = dead_rx.recv() => {
                break format!("device stopped pinging ({PING_MISSES} missed)");
            }
            // One device per pairing: refuse anything else on this path.
            Some(inc) = ep.accept() => inc.refuse(),
        }
    };
    on_event(TunnelEvent::Disconnected { reason });
    ep.close(VarInt::from_u32(0), b"");
    Ok(())
}

async fn accept_one(ep: &Endpoint) -> anyhow::Result<Connection> {
    let inc = ep
        .accept()
        .await
        .ok_or_else(|| anyhow!("endpoint closed"))?;
    tracing::debug!(from = %inc.remote_address(), "tunnel: device QUIC incoming");
    inc.await.context("device handshake failed")
}

async fn box_stream(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    target: SocketAddr,
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
    if !ok || ty[0] != STREAM_TCP {
        let _ = send.reset(VarInt::from_u32(1));
        let _ = recv.stop(VarInt::from_u32(1));
        return;
    }
    match TcpStream::connect(target).await {
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

/// Device side: connect QUIC to the box over the punched path and expose
/// `listen`; each accepted TCP connection becomes one stream. Returns `Ok`
/// when an established tunnel ends (connections queued on `listen` in the
/// meantime are served by the next call), `Err` if the handshake fails.
/// [`run_device`] wraps this in a reconnect loop with an accept filter.
pub async fn connect_device(
    path: PunchedPath,
    secret: &PairingSecret,
    listen: &TcpListener,
    on_event: impl Fn(TunnelEvent) + Send + Sync + 'static,
) -> anyhow::Result<()> {
    let never = CancellationToken::new();
    device_session(path, secret, listen, None, &never, on_event).await
}

/// [`connect_device`] plus an optional [`AcceptFilter`] run on every
/// accepted connection, and a `cancel` token that ends the tunnel (and
/// every stream on it) with `Disconnected { reason: "stopped" }`.
async fn device_session(
    path: PunchedPath,
    secret: &PairingSecret,
    listen: &TcpListener,
    filter: Option<&AcceptFilter>,
    cancel: &CancellationToken,
    on_event: impl Fn(TunnelEvent) + Send + Sync + 'static,
) -> anyhow::Result<()> {
    let handshake = tokio::select! {
        r = connect_raw(path, secret) => r,
        _ = cancel.cancelled() => return Ok(()),
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
    });
    let alive = device_liveness(conn.clone());
    tokio::pin!(alive);
    let reason = loop {
        tokio::select! {
            e = conn.closed() => break e.to_string(),
            r = &mut alive => break r,
            _ = cancel.cancelled() => break "stopped".to_string(),
            a = listen.accept() => match a {
                Ok((tcp, _)) => {
                    let conn = conn.clone();
                    let filter = filter.cloned();
                    tokio::spawn(async move {
                        let tcp = match filter {
                            Some(f) => match f(tcp).await {
                                Some(t) => t,
                                None => return,
                            },
                            None => tcp,
                        };
                        let Ok((mut send, recv)) = conn.open_bi().await else { return };
                        if send.write_all(&[STREAM_TCP]).await.is_ok() {
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

/// The device's authenticated QUIC connection, without the TCP plumbing.
/// Test hook; keep the returned endpoint alive with the connection.
#[doc(hidden)]
pub async fn connect_raw(
    path: PunchedPath,
    secret: &PairingSecret,
) -> anyhow::Result<(Endpoint, Connection)> {
    let peer = path.peer;
    tracing::debug!(%peer, "tunnel: QUIC connect");
    let ep = endpoint(path, None)?;
    let connecting = ep.connect_with(client_config(secret)?, peer, SERVER_NAME)?;
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
    fn link_roundtrip() {
        let s = PairingSecret::generate();
        let l = PairingLink::new(s.clone(), "relay.example:4443");
        let uri = l.to_uri();
        assert!(uri.starts_with("peckboard://pair/"));
        let p = PairingLink::parse(&uri).unwrap();
        assert_eq!(p.secret.as_bytes(), s.as_bytes());
        assert_eq!(p.relay, "relay.example:4443");
        let bare = PairingLink::parse(uri.split('?').next().unwrap()).unwrap();
        assert_eq!(bare.relay, DEFAULT_RELAY);
        assert!(PairingLink::parse("peckboard://pair/AAAA").is_err());
        assert!(PairingLink::parse("https://x/pair/AAAA").is_err());
    }

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
