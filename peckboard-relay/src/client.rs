//! Peer side of the rendezvous protocol, shared by the Peckboard box
//! (Phase 2) and `peckboard connect` (Phase 3).
//!
//! ```no_run
//! # async fn demo(secret: peckboard_relay::keys::PairingSecret) -> anyhow::Result<()> {
//! use peckboard_relay::client::{ClientConfig, Event, RelayClient};
//! use peckboard_relay::proto::Role;
//! let cfg = ClientConfig::webpki("relay.peckboard.com:443".parse()?, "relay.peckboard.com");
//! let mut c = RelayClient::connect(&cfg, &secret, Role::Device).await?;
//! let udp = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
//! c.stun_binding(&udp).await?;          // relay learns our public endpoint
//! while let Some(ev) = c.next_event().await {
//!     if let Event::Punch(p) = ev {
//!         let peer = c.punch(&udp, &p, std::time::Duration::from_secs(5)).await?;
//!         // talk to `peer` directly over `udp` from here on
//!         # let _ = peer; break;
//!     }
//! }
//! # Ok(()) }
//! ```

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow, bail};
use rand::RngCore;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig as TlsClientConfig, RootCertStore};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio_rustls::TlsConnector;

use crate::identity::{BoxIdentity, encode_key};
use crate::keys::{
    DerivedKeys, EXPORTER_LABEL, EXPORTER_LEN, MsgCounter, PairingSecret, ReplayGuard,
};
use crate::proto::{
    ALPN, ALPN_HTTP1, ALPN_V2, ALPN_V3, ClientMsg, MAX_BLOB, Role, ServerMsg, decode_addrs,
    encode_addrs, read_frame, write_frame,
};
use crate::stun;

const CTX_MSG: &[u8] = b"msg";
const CTX_CANDIDATES: &[u8] = b"cand";
const CTX_PUNCH: &[u8] = b"punch";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct ClientConfig {
    pub relay: SocketAddr,
    pub server_name: String,
    pub roots: Arc<RootCertStore>,
    /// STUN host; defaults to the relay's IP.
    pub stun_host: Option<IpAddr>,
    /// Speak protocol v1 only (no relay fallback) — behave like a client
    /// that predates it. Tests / troubleshooting.
    pub v1_only: bool,
}

impl ClientConfig {
    pub fn webpki(relay: SocketAddr, server_name: &str) -> Self {
        let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Self {
            relay,
            server_name: server_name.to_string(),
            roots: Arc::new(roots),
            stun_host: None,
            v1_only: false,
        }
    }

    /// Trust exactly `cert` (dev / tests with `--dev-self-signed`).
    pub fn pinned(
        relay: SocketAddr,
        server_name: &str,
        cert: CertificateDer<'static>,
    ) -> anyhow::Result<Self> {
        let mut roots = RootCertStore::empty();
        roots.add(cert)?;
        Ok(Self {
            relay,
            server_name: server_name.to_string(),
            roots: Arc::new(roots),
            stun_host: None,
            v1_only: false,
        })
    }
}

#[derive(Clone, Debug)]
pub struct StunCredential {
    pub username: String,
    pub password: String,
    pub server: SocketAddr,
    pub expires: Instant,
}

#[derive(Clone, Debug)]
pub struct Punch {
    pub peer_public: SocketAddr,
    pub start_at_ms: u64,
    pub nonce: [u8; 16],
    pub attempt: u8,
    /// The peer's self-reported candidates (decrypted); empty if none.
    pub peer_candidates: Vec<SocketAddr>,
}

#[derive(Debug)]
pub enum Event {
    PeerOnline,
    PeerOffline,
    /// E2E message from the peer. `sealed` is the exact blob the relay
    /// forwarded (what the relay could see).
    Message {
        plaintext: Vec<u8>,
        sealed: Vec<u8>,
    },
    Punch(Punch),
    CredentialRefreshed,
    Pong,
}

/// What a v3 relay said about this session's box identity
/// ([`RelayClient::identity_status`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdentityStatus {
    /// The identity this session proved is in the relay's registry. Always
    /// false for a session that presented none (devices, a box without an
    /// identity).
    pub registered: bool,
    /// The relay requires a registered box for relayed data right now.
    pub gated: bool,
}

impl IdentityStatus {
    /// May this session's pair use the relay data channel?
    pub fn relay_permitted(&self) -> bool {
        self.registered || !self.gated
    }
}

pub struct RelayClient {
    role: Role,
    keys: Arc<DerivedKeys>,
    out: mpsc::Sender<ClientMsg>,
    events: mpsc::Receiver<Event>,
    cred: Arc<Mutex<StunCredential>>,
    send_ctr: MsgCounter,
    version: u8,
    data: Option<RelayData>,
    identity_status: Option<IdentityStatus>,
}

/// Datagrams queued per direction on the v2 relay data channel.
const DATA_QUEUE: usize = 128;

/// The v2 relay data channel ([`RelayClient::take_data`]): opaque datagrams
/// to (`tx`) and from (`rx`) the peer, forwarded by the relay over this
/// session. Lives as long as the [`RelayClient`] it came from.
pub struct RelayData {
    pub tx: mpsc::Sender<Vec<u8>>,
    pub rx: mpsc::Receiver<Vec<u8>>,
}

fn tls_config(roots: Arc<RootCertStore>, v1_only: bool) -> anyhow::Result<Arc<TlsClientConfig>> {
    let mut cfg =
        TlsClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_root_certificates(roots)
            .with_no_client_auth();
    // An old relay only knows v1 (or v1+v2) and picks the best of those; a
    // current one prefers v3.
    cfg.alpn_protocols = if v1_only {
        vec![ALPN.to_vec()]
    } else {
        vec![ALPN_V3.to_vec(), ALPN_V2.to_vec(), ALPN.to_vec()]
    };
    Ok(Arc::new(cfg))
}

fn credential(stun_host: IpAddr, m: ServerMsg) -> Option<StunCredential> {
    match m {
        ServerMsg::Registered {
            stun_username,
            stun_password,
            ttl_secs,
            stun_port,
        } => Some(StunCredential {
            username: stun_username,
            password: stun_password,
            server: SocketAddr::new(stun_host, stun_port),
            expires: Instant::now() + Duration::from_secs(ttl_secs as u64),
        }),
        _ => None,
    }
}

impl RelayClient {
    pub async fn connect(
        cfg: &ClientConfig,
        secret: &PairingSecret,
        role: Role,
    ) -> anyhow::Result<Self> {
        Self::connect_with_identity(cfg, secret, role, None).await
    }

    /// [`connect`](Self::connect); a box on a v3 relay also proves its
    /// permanent `identity` (ignored for devices and older relays). See
    /// [`identity_status`](Self::identity_status) for the relay's answer.
    pub async fn connect_with_identity(
        cfg: &ClientConfig,
        secret: &PairingSecret,
        role: Role,
        identity: Option<&BoxIdentity>,
    ) -> anyhow::Result<Self> {
        let keys = Arc::new(secret.derive());
        let connector = TlsConnector::from(tls_config(cfg.roots.clone(), cfg.v1_only)?);
        let name = ServerName::try_from(cfg.server_name.clone()).context("server name")?;
        let stun_host = cfg.stun_host.unwrap_or(cfg.relay.ip());

        let hs = async {
            let tcp = TcpStream::connect(cfg.relay).await?;
            let _ = tcp.set_nodelay(true);
            let tls = connector.connect(name, tcp).await?;
            let mut exporter = [0u8; EXPORTER_LEN];
            let version = {
                let conn = tls.get_ref().1;
                let version = match conn.alpn_protocol() {
                    Some(p) if p == ALPN_V3 && !cfg.v1_only => 3u8,
                    Some(p) if p == ALPN_V2 && !cfg.v1_only => 2,
                    Some(p) if p == ALPN => 1,
                    Some(p) if p == ALPN => 1,
                    _ => bail!(
                        "relay did not negotiate {:?}",
                        String::from_utf8_lossy(ALPN)
                    ),
                };
                conn.export_keying_material(&mut exporter, EXPORTER_LABEL, None)?;
                version
            };
            let (mut rd, mut wr) = tokio::io::split(tls);
            let hello = ClientMsg::Hello {
                role,
                rendezvous_id: keys.rendezvous_id,
                public_key: keys.public_key(),
            };
            write_frame(&mut wr, &hello.encode()).await?;
            let nonce = match ServerMsg::decode(&read_frame(&mut rd).await?)? {
                ServerMsg::Challenge { nonce } => nonce,
                _ => bail!("expected challenge"),
            };
            let signature = keys.sign_challenge(&nonce, role, &exporter);
            let auth = match identity {
                Some(id) if role == Role::Box && version >= 3 => ClientMsg::IdentityAuth {
                    signature,
                    identity_key: id.public_key(),
                    identity_signature: id.sign_session(&nonce, &keys.rendezvous_id, &exporter),
                },
                _ => ClientMsg::Auth { signature },
            };
            write_frame(&mut wr, &auth.encode()).await?;
            let reg = ServerMsg::decode(&read_frame(&mut rd).await?)?;
            let cred = credential(stun_host, reg).ok_or_else(|| anyhow!("expected registered"))?;
            // v3: the identity verdict follows the first Registered.
            let status = if version >= 3 {
                match ServerMsg::decode(&read_frame(&mut rd).await?)? {
                    ServerMsg::IdentityStatus { registered, gated } => {
                        Some(IdentityStatus { registered, gated })
                    }
                    _ => bail!("expected identity status"),
                }
            } else {
                None
            };
            Ok((rd, wr, cred, version, status))
        };
        let (mut rd, mut wr, cred, version, identity_status) =
            tokio::time::timeout(HANDSHAKE_TIMEOUT, hs)
                .await
                .context("relay handshake timed out")??;

        let cred = Arc::new(Mutex::new(cred));
        let (out_tx, mut out_rx) = mpsc::channel::<ClientMsg>(64);
        let (ev_tx, ev_rx) = mpsc::channel::<Event>(256);
        // v2 data channel: separate queues so datagrams never hold up (or
        // get held up by) signaling, and drop instead of blocking when full.
        let (data_out_tx, mut data_out_rx) = mpsc::channel::<Vec<u8>>(DATA_QUEUE);
        let (data_in_tx, data_in_rx) = mpsc::channel::<Vec<u8>>(DATA_QUEUE);

        tokio::spawn(async move {
            loop {
                let m = tokio::select! {
                    biased;
                    m = out_rx.recv() => match m {
                        Some(m) => m,
                        None => break,
                    },
                    Some(packet) = data_out_rx.recv() => ClientMsg::Data { packet },
                };
                if write_frame(&mut wr, &m.encode()).await.is_err() {
                    break;
                }
            }
            // Client dropped: close_notify + FIN so the relay frees our slot.
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut wr).await;
        });

        let (k, c) = (keys.clone(), cred.clone());
        tokio::spawn(async move {
            let mut replay = ReplayGuard::default();
            while let Ok(frame) = read_frame(&mut rd).await {
                let Ok(m) = ServerMsg::decode(&frame) else {
                    break;
                };
                let ev = match m {
                    ServerMsg::Data { packet } => {
                        let _ = data_in_tx.try_send(packet);
                        continue;
                    }
                    ServerMsg::Pong => Event::Pong,
                    ServerMsg::PeerOnline => Event::PeerOnline,
                    ServerMsg::PeerOffline => Event::PeerOffline,
                    ServerMsg::Forwarded { blob } => {
                        let now_ms = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map_or(0, |d| d.as_millis() as u64);
                        match k
                            .e2e
                            .open_counted(role.other(), CTX_MSG, &blob, &mut replay, now_ms)
                        {
                            Some(plaintext) => Event::Message {
                                plaintext,
                                sealed: blob,
                            },
                            None => continue, // not from our peer, or a replay
                        }
                    }
                    ServerMsg::PunchNow {
                        peer_public,
                        start_at_ms,
                        nonce,
                        attempt,
                        peer_candidates,
                    } => {
                        let peer_candidates = k
                            .e2e
                            .open(role.other(), CTX_CANDIDATES, &peer_candidates)
                            .and_then(|pt| decode_addrs(&pt))
                            .unwrap_or_default();
                        Event::Punch(Punch {
                            peer_public,
                            start_at_ms,
                            nonce,
                            attempt,
                            peer_candidates,
                        })
                    }
                    reg @ ServerMsg::Registered { .. } => {
                        if let Some(nc) = credential(stun_host, reg) {
                            *c.lock().unwrap() = nc;
                        }
                        Event::CredentialRefreshed
                    }
                    ServerMsg::Challenge { .. } => break,
                    // Only sent once, during the handshake.
                    ServerMsg::IdentityStatus { .. } => continue,
                };
                if ev_tx.send(ev).await.is_err() {
                    break;
                }
            }
        });

        Ok(Self {
            role,
            keys,
            out: out_tx,
            events: ev_rx,
            cred,
            send_ctr: MsgCounter::default(),
            version,
            data: (version >= 2).then_some(RelayData {
                tx: data_out_tx,
                rx: data_in_rx,
            }),
            identity_status,
        })
    }

    /// The v3 relay's verdict on this session's box identity, as of the
    /// handshake; `None` on an older relay (v1/v2), which knows nothing of
    /// identities and never gates. A box on a gated relay that reports
    /// `registered: false` may still rendezvous and punch, but not relay.
    pub fn identity_status(&self) -> Option<IdentityStatus> {
        self.identity_status
    }

    pub fn role(&self) -> Role {
        self.role
    }

    /// Next relay event; `None` once the relay connection is gone.
    pub async fn next_event(&mut self) -> Option<Event> {
        self.events.recv().await
    }

    async fn push(&self, m: ClientMsg) -> anyhow::Result<()> {
        self.out
            .send(m)
            .await
            .map_err(|_| anyhow!("relay connection closed"))
    }

    /// Seal `plaintext` for the peer and hand it to the relay. Carries a
    /// per-sender counter + timestamp so the peer drops replays.
    pub async fn send(&self, plaintext: &[u8]) -> anyhow::Result<()> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
        let counter = self.send_ctr.next(now.as_micros() as u64);
        let blob = self.keys.e2e.seal_counted(
            self.role,
            CTX_MSG,
            counter,
            now.as_millis() as u64,
            plaintext,
        );
        if blob.len() > MAX_BLOB {
            bail!("message too large");
        }
        self.push(ClientMsg::Forward { blob }).await
    }

    /// Report extra (e.g. LAN) candidates, sealed so the relay can't read them.
    pub async fn set_candidates(&self, addrs: &[SocketAddr]) -> anyhow::Result<()> {
        let blob = self
            .keys
            .e2e
            .seal(self.role, CTX_CANDIDATES, &encode_addrs(addrs));
        self.push(ClientMsg::SetCandidates { blob }).await
    }

    pub async fn request_punch(&self) -> anyhow::Result<()> {
        self.push(ClientMsg::PunchRequest).await
    }

    pub async fn ping(&self) -> anyhow::Result<()> {
        self.push(ClientMsg::Ping).await
    }

    pub async fn refresh_stun(&self) -> anyhow::Result<()> {
        self.push(ClientMsg::RefreshStun).await
    }

    pub fn stun_credential(&self) -> StunCredential {
        self.cred.lock().unwrap().clone()
    }

    /// Negotiated protocol version: 3 when the relay knows box identities
    /// ([`crate::proto::ALPN_V3`]), 2 when it offers the relay data channel
    /// ([`crate::proto::ALPN_V2`]), 1 for an older relay (or
    /// [`ClientConfig::v1_only`]).
    pub fn protocol_version(&self) -> u8 {
        self.version
    }

    /// The v2 relay data channel, once (`None` on a v1 session or when
    /// already taken). Datagrams sent on `tx` reach the peer's `rx` verbatim
    /// through the relay; both queues drop when full, like UDP.
    pub fn take_data(&mut self) -> Option<RelayData> {
        self.data.take()
    }

    /// Authenticated STUN Binding from `sock`; returns our public endpoint
    /// as the relay sees it (and lets the relay record it for punching).
    pub async fn stun_binding(&self, sock: &UdpSocket) -> anyhow::Result<SocketAddr> {
        let cred = self.stun_credential();
        for _ in 0..4 {
            let mut txid = [0u8; 12];
            rand::rngs::OsRng.fill_bytes(&mut txid);
            let req = stun::build_request(&txid, &cred.username, cred.password.as_bytes());
            sock.send_to(&req, cred.server).await?;
            let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
            let mut buf = [0u8; stun::MAX_PACKET];
            while let Ok(r) = tokio::time::timeout_at(deadline, sock.recv_from(&mut buf)).await {
                let (n, from) = r?;
                if from != cred.server {
                    continue;
                }
                if let Some(a) = stun::parse_response(&buf[..n], &txid, cred.password.as_bytes()) {
                    return Ok(a);
                }
            }
        }
        bail!("no STUN response")
    }

    /// Simultaneous-open hole punch. Sends sealed probes to the peer's
    /// public endpoint and candidates from `start_at_ms`, acks every address
    /// a valid peer probe arrives from, and returns the first address an ack
    /// arrives from — a path proven to work both ways. A peer that never
    /// acks (older version) gets its first probe address after `ACK_GRACE`.
    pub async fn punch(
        &self,
        sock: &UdpSocket,
        p: &Punch,
        timeout: Duration,
    ) -> anyhow::Result<SocketAddr> {
        punch_with(&self.keys, self.role, sock, p, timeout).await
    }
}
/// Ask the relay whether box identity `key` is registered: one HTTPS
/// `GET /api/registered?key=…` on the relay's signaling port (ALPN
/// `http/1.1`, same certificate trust as [`RelayClient::connect`]). Cheap
/// enough to poll every few seconds while a user is on the registration
/// page (the relay allows ~1 request / 2 s per IP, burst 20). Errors on a
/// relay that predates registration (it closes such connections).
pub async fn registration_status(cfg: &ClientConfig, key: &[u8; 32]) -> anyhow::Result<bool> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut tls_cfg =
        TlsClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_root_certificates(cfg.roots.clone())
            .with_no_client_auth();
    tls_cfg.alpn_protocols = vec![ALPN_HTTP1.to_vec()];
    let connector = TlsConnector::from(Arc::new(tls_cfg));
    let name = ServerName::try_from(cfg.server_name.clone()).context("server name")?;
    let req = async {
        let tcp = TcpStream::connect(cfg.relay).await?;
        let mut tls = connector.connect(name, tcp).await?;
        let head = format!(
            "GET /api/registered?key={} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            encode_key(key),
            cfg.server_name
        );
        tls.write_all(head.as_bytes()).await?;
        let mut resp = Vec::new();
        (&mut tls).take(16 * 1024).read_to_end(&mut resp).await?;
        anyhow::Ok(resp)
    };
    let resp = tokio::time::timeout(HANDSHAKE_TIMEOUT, req)
        .await
        .context("relay registration check timed out")??;
    let resp = String::from_utf8_lossy(&resp);
    let (head, body) = resp
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow!("relay sent no HTTP response"))?;
    let status = head.split(' ').nth(1).unwrap_or("");
    if status != "200" {
        bail!("relay registration check: HTTP {status}");
    }
    let body: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    if body.contains("\"registered\":true") {
        Ok(true)
    } else if body.contains("\"registered\":false") {
        Ok(false)
    } else {
        bail!("relay registration check: unexpected body")
    }
}

/// Probe payload (sealed). Peers that predate acks send `b"probe"` and
/// accept any valid seal as a probe (so our acks count as probes for them);
/// this one also says "I ack". Earlier ack-capable peers treat any non-ACK
/// seal as a probe, so it is wire-compatible both ways.
const PROBE_ACKS: &[u8] = b"probe+ack";
/// "Your probe reached me here" — sent to every address a probe came from.
const ACK: &[u8] = b"ack";
/// How long a heard-but-unacked probe path waits before it's used anyway
/// (peers that predate acks never send one).
const ACK_GRACE: Duration = Duration::from_millis(1500);
const MAX_HEARD: usize = 8;

fn punch_ctx(nonce: &[u8; 16]) -> Vec<u8> {
    let mut c = CTX_PUNCH.to_vec();
    c.extend_from_slice(nonce);
    c
}

async fn punch_with(
    keys: &DerivedKeys,
    role: Role,
    sock: &UdpSocket,
    p: &Punch,
    timeout: Duration,
) -> anyhow::Result<SocketAddr> {
    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    let wait = p.start_at_ms.saturating_sub(now_ms).min(5_000);
    tokio::time::sleep(Duration::from_millis(wait)).await;

    let ctx = punch_ctx(&p.nonce);
    let mut targets = vec![p.peer_public];
    targets.extend(
        p.peer_candidates
            .iter()
            .copied()
            .filter(|a| *a != p.peer_public),
    );
    let deadline = tokio::time::Instant::now() + timeout;
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut buf = [0u8; 512];
    let mut foreign = 0u32;
    // Addresses a valid peer probe arrived from, when, and whether that peer
    // acks. The peer reaches us from there, but that alone doesn't prove the
    // way back (a multi-homed host may route its replies out another
    // interface; a symmetric NAT may let probes in but not our answers out).
    let mut heard: Vec<(SocketAddr, tokio::time::Instant, bool)> = Vec::new();
    // Only a peer that never acks gets the unproven-path fallback: from one
    // that does, silence means our side of the path is dead.
    let legacy = |heard: &[(SocketAddr, tokio::time::Instant, bool)]| {
        heard
            .iter()
            .find(|(_, _, acks)| !acks)
            .map(|&(a, at, _)| (a, at))
    };
    tracing::debug!(?role, attempt = p.attempt, ?targets, "punch: probing");
    let chosen = loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                if let Some((from, _)) = legacy(&heard) {
                    break from;
                }
                tracing::debug!(?role, foreign, heard = heard.len(), "punch: timed out");
                bail!("hole punch timed out")
            }
            _ = tick.tick() => {
                if let Some((from, at)) = legacy(&heard)
                    && at.elapsed() >= ACK_GRACE
                {
                    tracing::debug!(?role, %from, "punch: no ack, using first probe path");
                    break from;
                }
                let probe = keys.e2e.seal(role, &ctx, PROBE_ACKS);
                for t in &targets {
                    let _ = sock.send_to(&probe, t).await;
                }
                let ack = keys.e2e.seal(role, &ctx, ACK);
                for (h, _, _) in &heard {
                    let _ = sock.send_to(&ack, h).await;
                }
            }
            r = sock.recv_from(&mut buf) => {
                let Ok((n, from)) = r else { continue };
                let Some(msg) = keys.e2e.open(role.other(), &ctx, &buf[..n]) else {
                    foreign += 1;
                    continue;
                };
                if msg == ACK {
                    // Our probe got there and its answer got back: two-way.
                    tracing::debug!(?role, %from, "punch: peer ack received");
                    break from;
                }
                if !heard.iter().any(|(h, _, _)| *h == from) && heard.len() < MAX_HEARD {
                    tracing::debug!(?role, %from, "punch: peer probe received");
                    heard.push((from, tokio::time::Instant::now(), msg == PROBE_ACKS));
                    let ack = keys.e2e.seal(role, &ctx, ACK);
                    let _ = sock.send_to(&ack, from).await;
                }
            }
        }
    };
    // Let the peer finish too: it may not have our ack yet.
    for _ in 0..3 {
        let ack = keys.e2e.seal(role, &ctx, ACK);
        let _ = sock.send_to(&ack, chosen).await;
    }
    Ok(chosen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::PairingSecret;

    fn punch_to(peer_public: SocketAddr, start_at_ms: u64) -> Punch {
        Punch {
            peer_public,
            start_at_ms,
            nonce: [7; 16],
            attempt: 1,
            peer_candidates: vec![],
        }
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    /// A path that only carries device→box (e.g. a multi-homed box that
    /// routes LAN replies out another interface) must not win just because
    /// its probe arrives first: the box would then wait for QUIC on an
    /// address the device never hears.
    #[tokio::test]
    async fn punch_ignores_one_way_path() {
        let keys = PairingSecret::generate().derive();
        let ub = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let ud = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // One-way forwarder: device→box only, from its own address; what
        // the box sends back to it is lost.
        let fwd = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (b, d, f) = (
            ub.local_addr().unwrap(),
            ud.local_addr().unwrap(),
            fwd.local_addr().unwrap(),
        );
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            while let Ok((n, from)) = fwd.recv_from(&mut buf).await {
                if from == d {
                    let _ = fwd.send_to(&buf[..n], b).await;
                }
            }
        });
        // The device starts first and only knows the forwarder, so the box
        // hears it on the one-way path first; the box probes the device's
        // real address a little later.
        let start = now_ms();
        let pd = punch_to(f, start);
        let pb = punch_to(d, start + 300);
        let t = Duration::from_secs(3);
        let (rb, rd) = tokio::join!(
            punch_with(&keys, Role::Box, &ub, &pb, t),
            punch_with(&keys, Role::Device, &ud, &pd, t),
        );
        assert_eq!(rb.unwrap(), d, "box must pick the two-way path");
        assert_eq!(rd.unwrap(), b);
    }

    /// Normal path, both sides current: no extra wait over a probe RTT.
    #[tokio::test]
    async fn punch_two_way_is_fast() {
        let keys = PairingSecret::generate().derive();
        let ub = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let ud = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (b, d) = (ub.local_addr().unwrap(), ud.local_addr().unwrap());
        let (pb, pd) = (punch_to(d, now_ms()), punch_to(b, now_ms()));
        let t0 = Instant::now();
        let t = Duration::from_secs(3);
        let (rb, rd) = tokio::join!(
            punch_with(&keys, Role::Box, &ub, &pb, t),
            punch_with(&keys, Role::Device, &ud, &pd, t),
        );
        assert_eq!((rb.unwrap(), rd.unwrap()), (d, b));
        assert!(
            t0.elapsed() < Duration::from_millis(500),
            "{:?}",
            t0.elapsed()
        );
    }

    /// An older peer probes but never acks; we still settle on the path its
    /// probes arrive from, after a short grace.
    #[tokio::test]
    async fn punch_falls_back_for_peer_without_acks() {
        let s = PairingSecret::generate();
        let keys = s.derive();
        let ub = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let ud = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (b, d) = (ub.local_addr().unwrap(), ud.local_addr().unwrap());
        let ctx = punch_ctx(&[7; 16]);
        let legacy_keys = s.derive();
        // Pre-ack behaviour: probe every 100 ms, take the first valid
        // packet's source, send three more probes there.
        let legacy = tokio::spawn(async move {
            let mut buf = [0u8; 512];
            loop {
                let probe = legacy_keys.e2e.seal(Role::Device, &ctx, b"probe");
                ud.send_to(&probe, b).await.unwrap();
                if let Ok(Ok((n, from))) =
                    tokio::time::timeout(Duration::from_millis(100), ud.recv_from(&mut buf)).await
                    && legacy_keys.e2e.open(Role::Box, &ctx, &buf[..n]).is_some()
                {
                    for _ in 0..3 {
                        let probe = legacy_keys.e2e.seal(Role::Device, &ctx, b"probe");
                        let _ = ud.send_to(&probe, from).await;
                    }
                    return from;
                }
            }
        });
        let rb = punch_with(
            &keys,
            Role::Box,
            &ub,
            &punch_to(d, now_ms()),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(rb.unwrap(), d);
        assert_eq!(legacy.await.unwrap(), b);
    }

    /// Only device→box works (box symmetric NAT × carrier CGNAT): the box
    /// hears the device's probes but nothing it sends gets back. A current
    /// peer acks, so its silence means one-way — the box must fail the round
    /// instead of taking the grace fallback, or it reports "connected" and
    /// sits out the next rounds waiting 15 s for a QUIC handshake that
    /// cannot arrive.
    #[tokio::test]
    async fn punch_rejects_one_way_only_path() {
        let keys = PairingSecret::generate().derive();
        let ub = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let ud = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let fwd = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // Where the box thinks the device is; nothing ever comes back out.
        let sink = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (b, d, f) = (
            ub.local_addr().unwrap(),
            ud.local_addr().unwrap(),
            fwd.local_addr().unwrap(),
        );
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            while let Ok((n, from)) = fwd.recv_from(&mut buf).await {
                if from == d {
                    let _ = fwd.send_to(&buf[..n], b).await;
                }
            }
        });
        let pb = punch_to(sink.local_addr().unwrap(), now_ms());
        let pd = punch_to(f, now_ms());
        let t = Duration::from_secs(3);
        let (rb, rd) = tokio::join!(
            punch_with(&keys, Role::Box, &ub, &pb, t),
            punch_with(&keys, Role::Device, &ud, &pd, t),
        );
        assert!(rb.is_err(), "box took a one-way path: {rb:?}");
        assert!(rd.is_err(), "{rd:?}");
        drop(sink);
    }

    /// Test NAT in front of one host socket. The host reaches an outside
    /// address `o` through an inside alias socket ([`Nat::alias`]); the NAT
    /// sends it on from an outside mapping — one per destination when
    /// `symmetric` (endpoint-dependent mapping), else one shared mapping.
    /// Filtering is address-and-port dependent either way: a mapping only
    /// lets in what comes from an address it has sent to. `forward` is a
    /// router port-forward: anyone may send to it, and replies to such a
    /// sender leave from it (like a conntrack entry for the inbound flow).
    struct Nat {
        host: SocketAddr,
        symmetric: bool,
        st: tokio::sync::Mutex<NatState>,
    }

    #[derive(Default)]
    struct NatState {
        aliases: std::collections::HashMap<SocketAddr, Arc<UdpSocket>>,
        mappings: std::collections::HashMap<Option<SocketAddr>, Arc<NatMapping>>,
        forward: Option<Arc<UdpSocket>>,
        via_forward: std::collections::HashSet<SocketAddr>,
    }

    struct NatMapping {
        sock: UdpSocket,
        sent_to: Mutex<std::collections::HashSet<SocketAddr>>,
    }

    impl Nat {
        async fn new(host: SocketAddr, symmetric: bool, forward: bool) -> Arc<Self> {
            let nat = Arc::new(Self {
                host,
                symmetric,
                st: Default::default(),
            });
            if forward {
                let f = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
                nat.st.lock().await.forward = Some(f.clone());
                let n = nat.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    while let Ok((len, src)) = f.recv_from(&mut buf).await {
                        n.st.lock().await.via_forward.insert(src);
                        n.deliver(src, &buf[..len]).await;
                    }
                });
            }
            nat
        }

        async fn forward_addr(&self) -> SocketAddr {
            let st = self.st.lock().await;
            st.forward.as_ref().unwrap().local_addr().unwrap()
        }

        /// The inside address the host uses to reach outside address `o`.
        async fn alias(self: &Arc<Self>, o: SocketAddr) -> SocketAddr {
            let mut st = self.st.lock().await;
            if let Some(a) = st.aliases.get(&o) {
                return a.local_addr().unwrap();
            }
            let a = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
            st.aliases.insert(o, a.clone());
            let n = self.clone();
            let sock = a.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                while let Ok((len, src)) = sock.recv_from(&mut buf).await {
                    if src == n.host {
                        n.outbound(o, &buf[..len]).await;
                    }
                }
            });
            a.local_addr().unwrap()
        }

        async fn outbound(self: &Arc<Self>, o: SocketAddr, data: &[u8]) {
            let mut st = self.st.lock().await;
            if st.via_forward.contains(&o) {
                let f = st.forward.clone().unwrap();
                drop(st);
                let _ = f.send_to(data, o).await;
                return;
            }
            let key = self.symmetric.then_some(o);
            let m = match st.mappings.get(&key) {
                Some(m) => m.clone(),
                None => {
                    let m = Arc::new(NatMapping {
                        sock: UdpSocket::bind("127.0.0.1:0").await.unwrap(),
                        sent_to: Mutex::default(),
                    });
                    st.mappings.insert(key, m.clone());
                    let (n, mm) = (self.clone(), m.clone());
                    tokio::spawn(async move {
                        let mut buf = [0u8; 2048];
                        while let Ok((len, src)) = mm.sock.recv_from(&mut buf).await {
                            if mm.sent_to.lock().unwrap().contains(&src) {
                                n.deliver(src, &buf[..len]).await;
                            }
                        }
                    });
                    m
                }
            };
            drop(st);
            m.sent_to.lock().unwrap().insert(o);
            let _ = m.sock.send_to(data, o).await;
        }

        /// Inbound from outside `src`: hand it to the host from `src`'s alias.
        /// Boxed: `alias` → `outbound` → `deliver` → `alias` is a cycle.
        fn deliver<'a>(
            self: &'a Arc<Self>,
            src: SocketAddr,
            data: &'a [u8],
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
            Box::pin(async move {
                self.alias(src).await;
                let a = self.st.lock().await.aliases.get(&src).cloned().unwrap();
                let _ = a.send_to(data, self.host).await;
            })
        }
    }

    /// Our public endpoint behind `nat`, as a STUN server at `stun` sees it.
    async fn observed(nat: &Arc<Nat>, host: &UdpSocket, stun: &UdpSocket) -> SocketAddr {
        host.send_to(b"stun", nat.alias(stun.local_addr().unwrap()).await)
            .await
            .unwrap();
        let mut buf = [0u8; 16];
        let (_, from) = tokio::time::timeout(Duration::from_secs(2), stun.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        from
    }

    /// A box behind a symmetric NAT (new public port per destination) and a
    /// device behind a port-restricted cone can't punch: each side's probes
    /// come from a port the other's NAT never opened. A port-forward on the
    /// box's router, advertised as a candidate, gets through — the device
    /// probes it like any other candidate and the box's acks leave from it.
    #[tokio::test]
    async fn advertised_forward_beats_symmetric_nat() {
        async fn run(advertise: bool) -> (anyhow::Result<SocketAddr>, anyhow::Result<SocketAddr>) {
            let keys = PairingSecret::generate().derive();
            let ub = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let ud = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let stun = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let box_nat = Nat::new(ub.local_addr().unwrap(), true, true).await;
            let dev_nat = Nat::new(ud.local_addr().unwrap(), false, false).await;
            let box_public = observed(&box_nat, &ub, &stun).await;
            let dev_public = observed(&dev_nat, &ud, &stun).await;
            let mut pd = punch_to(dev_nat.alias(box_public).await, now_ms());
            if advertise {
                let fwd = box_nat.forward_addr().await;
                pd.peer_candidates = vec![dev_nat.alias(fwd).await];
            }
            let pb = punch_to(box_nat.alias(dev_public).await, now_ms());
            let t = Duration::from_secs(2);
            tokio::join!(
                punch_with(&keys, Role::Box, &ub, &pb, t),
                punch_with(&keys, Role::Device, &ud, &pd, t),
            )
        }
        let (rb, rd) = run(false).await;
        assert!(rb.is_err() && rd.is_err(), "{rb:?} {rd:?}");
        let (rb, rd) = run(true).await;
        assert!(rb.is_ok() && rd.is_ok(), "{rb:?} {rd:?}");
    }
}
