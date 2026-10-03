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

use crate::keys::{
    DerivedKeys, EXPORTER_LABEL, EXPORTER_LEN, MsgCounter, PairingSecret, ReplayGuard,
};
use crate::proto::{
    ALPN, ClientMsg, MAX_BLOB, Role, ServerMsg, decode_addrs, encode_addrs, read_frame, write_frame,
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
}

impl ClientConfig {
    pub fn webpki(relay: SocketAddr, server_name: &str) -> Self {
        let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Self {
            relay,
            server_name: server_name.to_string(),
            roots: Arc::new(roots),
            stun_host: None,
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

pub struct RelayClient {
    role: Role,
    keys: Arc<DerivedKeys>,
    out: mpsc::Sender<ClientMsg>,
    events: mpsc::Receiver<Event>,
    cred: Arc<Mutex<StunCredential>>,
    send_ctr: MsgCounter,
}

fn tls_config(roots: Arc<RootCertStore>) -> anyhow::Result<Arc<TlsClientConfig>> {
    let mut cfg =
        TlsClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_root_certificates(roots)
            .with_no_client_auth();
    cfg.alpn_protocols = vec![ALPN.to_vec()];
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
        let keys = Arc::new(secret.derive());
        let connector = TlsConnector::from(tls_config(cfg.roots.clone())?);
        let name = ServerName::try_from(cfg.server_name.clone()).context("server name")?;
        let stun_host = cfg.stun_host.unwrap_or(cfg.relay.ip());

        let hs = async {
            let tcp = TcpStream::connect(cfg.relay).await?;
            let _ = tcp.set_nodelay(true);
            let tls = connector.connect(name, tcp).await?;
            let mut exporter = [0u8; EXPORTER_LEN];
            {
                let conn = tls.get_ref().1;
                if conn.alpn_protocol() != Some(ALPN) {
                    bail!(
                        "relay did not negotiate {:?}",
                        String::from_utf8_lossy(ALPN)
                    );
                }
                conn.export_keying_material(&mut exporter, EXPORTER_LABEL, None)?;
            }
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
            write_frame(&mut wr, &ClientMsg::Auth { signature }.encode()).await?;
            let reg = ServerMsg::decode(&read_frame(&mut rd).await?)?;
            let cred = credential(stun_host, reg).ok_or_else(|| anyhow!("expected registered"))?;
            Ok((rd, wr, cred))
        };
        let (mut rd, mut wr, cred) = tokio::time::timeout(HANDSHAKE_TIMEOUT, hs)
            .await
            .context("relay handshake timed out")??;

        let cred = Arc::new(Mutex::new(cred));
        let (out_tx, mut out_rx) = mpsc::channel::<ClientMsg>(64);
        let (ev_tx, ev_rx) = mpsc::channel::<Event>(256);

        tokio::spawn(async move {
            while let Some(m) = out_rx.recv().await {
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
        })
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

/// Probe payload (sealed). A pre-ack peer accepts any valid seal as a probe,
/// so our acks still count as probes for it.
const PROBE: &[u8] = b"probe";
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
    // Addresses a valid peer probe arrived from, and when. The peer reaches
    // us from there, but that alone doesn't prove the way back (a
    // multi-homed host may route its replies out another interface).
    let mut heard: Vec<(SocketAddr, tokio::time::Instant)> = Vec::new();
    tracing::debug!(?role, attempt = p.attempt, ?targets, "punch: probing");
    let chosen = loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                if let Some(&(from, _)) = heard.first() {
                    break from;
                }
                tracing::debug!(?role, foreign, "punch: timed out");
                bail!("hole punch timed out")
            }
            _ = tick.tick() => {
                if let Some(&(from, at)) = heard.first()
                    && at.elapsed() >= ACK_GRACE
                {
                    tracing::debug!(?role, %from, "punch: no ack, using first probe path");
                    break from;
                }
                let probe = keys.e2e.seal(role, &ctx, PROBE);
                for t in &targets {
                    let _ = sock.send_to(&probe, t).await;
                }
                let ack = keys.e2e.seal(role, &ctx, ACK);
                for (h, _) in &heard {
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
                if !heard.iter().any(|(h, _)| *h == from) && heard.len() < MAX_HEARD {
                    tracing::debug!(?role, %from, "punch: peer probe received");
                    heard.push((from, tokio::time::Instant::now()));
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
}
