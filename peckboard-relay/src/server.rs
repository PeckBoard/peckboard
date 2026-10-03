//! The rendezvous relay.
//!
//! State is memory-only: `rendezvous id → (public key, two peer slots)`
//! plus the short-lived STUN credentials. Nothing touches disk; a restart
//! forgets everything and peers simply re-register.
//!
//! Indistinguishability: a connection whose Hello/Auth is malformed, names
//! an unknown id it can't prove, or carries a bad signature is put in
//! *decoy* mode. A decoy session sees exactly what a legitimate peer whose
//! partner is offline sees — the same Challenge, a Registered (with a
//! working STUN credential) released at the same fixed delay after Auth,
//! Pong for Ping — and nothing else, ever. Only framing violations (an
//! oversize length prefix, EOF) close early, identically in both modes.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rand::RngCore;
use subtle::ConstantTimeEq;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{debug, info, warn};

use crate::keys::{EXPORTER_LABEL, EXPORTER_LEN, SEALED_OVERHEAD, verify_auth};
use crate::limits::{ConnCounter, RateLimiter, ip_tag};
use crate::proto::{
    ALPN, ClientMsg, MAX_BLOB, ProtoError, Role, ServerMsg, read_frame, write_frame,
};
use crate::stun;

#[derive(Clone, Debug)]
pub struct RelayConfig {
    pub max_connections: usize,
    pub max_connections_per_ip: usize,
    /// New TCP connections per second per IP (v6 /64) and burst.
    pub conn_rate_per_ip: f64,
    pub conn_burst_per_ip: f64,
    pub conn_rate_global: f64,
    pub conn_burst_global: f64,
    /// STUN packets per second per IP and burst.
    pub stun_rate_per_ip: f64,
    pub stun_burst_per_ip: f64,
    pub stun_rate_global: f64,
    pub stun_burst_global: f64,
    /// Signaling messages per second per session and burst.
    pub msg_rate: f64,
    pub msg_burst: f64,
    /// Cap on remembered rendezvous ids.
    pub max_ids: usize,
    /// How long an id with no peers online stays remembered.
    pub id_ttl: Duration,
    /// TLS handshake + Hello + Auth must finish within this.
    pub handshake_timeout: Duration,
    /// Close a session after this long without a frame from the peer.
    pub idle_timeout: Duration,
    /// Registered is released exactly this long after Auth arrives, whatever
    /// the outcome — masks signature/lookup timing.
    pub auth_delay: Duration,
    pub stun_credential_ttl: Duration,
    /// `PunchNow.start_at_ms` = now + this.
    pub punch_lead: Duration,
    pub max_punch_rounds: u8,
    pub punch_min_interval: Duration,
    /// Write full client IPs in logs (default: salted 4-byte hash).
    pub log_full_ips: bool,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            max_connections: 4096,
            max_connections_per_ip: 16,
            conn_rate_per_ip: 0.5,
            conn_burst_per_ip: 20.0,
            conn_rate_global: 200.0,
            conn_burst_global: 1000.0,
            stun_rate_per_ip: 5.0,
            stun_burst_per_ip: 40.0,
            stun_rate_global: 2000.0,
            stun_burst_global: 5000.0,
            msg_rate: 10.0,
            msg_burst: 40.0,
            max_ids: 100_000,
            id_ttl: Duration::from_secs(24 * 3600),
            handshake_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(90),
            auth_delay: Duration::from_millis(150),
            stun_credential_ttl: Duration::from_secs(600),
            punch_lead: Duration::from_millis(500),
            max_punch_rounds: 8,
            punch_min_interval: Duration::from_secs(1),
            log_full_ips: false,
        }
    }
}

struct Slot {
    conn_id: u64,
    tx: mpsc::Sender<ServerMsg>,
    kick: CancellationToken,
    public: Option<SocketAddr>,
    candidates: Vec<u8>,
}

struct IdEntry {
    public_key: [u8; 32],
    slots: [Option<Slot>; 2],
    idle_since: Instant,
    punch_rounds: u8,
    last_punch: Option<Instant>,
}

#[derive(Clone, Copy)]
struct Binding {
    rendezvous_id: [u8; 32],
    role: Role,
    conn_id: u64,
}

struct Cred {
    password: String,
    expires: Instant,
    /// None for decoy sessions: STUN answers, but nothing is recorded.
    bind: Option<Binding>,
}

struct Shared {
    cfg: RelayConfig,
    stun_port: u16,
    ids: Mutex<HashMap<[u8; 32], IdEntry>>,
    creds: Mutex<HashMap<String, Cred>>,
    conn_limiter: RateLimiter,
    stun_limiter: RateLimiter,
    per_ip: ConnCounter,
    active: AtomicUsize,
    next_conn: AtomicU64,
    salt: [u8; 16],
    shutdown: CancellationToken,
    tasks: TaskTracker,
}

/// Handle to a running relay. Cheap to clone.
#[derive(Clone)]
pub struct Relay {
    shared: Arc<Shared>,
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Relay {
    /// `stun_port` is the UDP port advertised to peers in `Registered`.
    pub fn new(cfg: RelayConfig, stun_port: u16) -> Self {
        let shared = Shared {
            conn_limiter: RateLimiter::new(
                cfg.conn_rate_per_ip,
                cfg.conn_burst_per_ip,
                cfg.conn_rate_global,
                cfg.conn_burst_global,
            ),
            stun_limiter: RateLimiter::new(
                cfg.stun_rate_per_ip,
                cfg.stun_burst_per_ip,
                cfg.stun_rate_global,
                cfg.stun_burst_global,
            ),
            cfg,
            stun_port,
            ids: Mutex::new(HashMap::new()),
            creds: Mutex::new(HashMap::new()),
            per_ip: ConnCounter::default(),
            active: AtomicUsize::new(0),
            next_conn: AtomicU64::new(1),
            salt: random(),
            shutdown: CancellationToken::new(),
            tasks: TaskTracker::new(),
        };
        let relay = Relay {
            shared: Arc::new(shared),
        };
        relay.spawn_housekeeping();
        relay
    }

    /// Stop accepting, close every session, and wait (bounded) for tasks.
    pub async fn shutdown(&self, grace: Duration) {
        self.shared.shutdown.cancel();
        self.shared.tasks.close();
        let _ = tokio::time::timeout(grace, self.shared.tasks.wait()).await;
    }

    /// Number of remembered rendezvous ids (tests / local admin only —
    /// never exposed over the network).
    pub fn id_count(&self) -> usize {
        self.shared.ids.lock().unwrap().len()
    }

    fn spawn_housekeeping(&self) {
        let shared = self.shared.clone();
        self.shared.tasks.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                tokio::select! {
                    _ = shared.shutdown.cancelled() => break,
                    _ = tick.tick() => {}
                }
                shared.conn_limiter.prune();
                shared.stun_limiter.prune();
                let now = Instant::now();
                shared.creds.lock().unwrap().retain(|_, c| c.expires > now);
                let ttl = shared.cfg.id_ttl;
                shared.ids.lock().unwrap().retain(|_, e| {
                    e.slots.iter().any(Option::is_some)
                        || now.saturating_duration_since(e.idle_since) < ttl
                });
            }
        });
    }

    fn tag(&self, ip: IpAddr) -> String {
        if self.shared.cfg.log_full_ips {
            ip.to_string()
        } else {
            ip_tag(&self.shared.salt, ip)
        }
    }

    // ---- TLS signaling ----------------------------------------------

    pub async fn serve_tls(&self, listener: TcpListener, acceptor: TlsAcceptor) {
        loop {
            let accepted = tokio::select! {
                _ = self.shared.shutdown.cancelled() => break,
                a = listener.accept() => a,
            };
            let (tcp, peer) = match accepted {
                Ok(x) => x,
                Err(e) => {
                    debug!("accept error: {e}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            let ip = peer.ip();
            let s = &self.shared;
            // Over-limit connections get a bare close: no TLS, no bytes.
            if !s.conn_limiter.allow(ip) {
                continue;
            }
            if s.active.fetch_add(1, Ordering::SeqCst) >= s.cfg.max_connections {
                s.active.fetch_sub(1, Ordering::SeqCst);
                continue;
            }
            if !s.per_ip.try_acquire(ip, s.cfg.max_connections_per_ip) {
                s.active.fetch_sub(1, Ordering::SeqCst);
                continue;
            }
            let relay = self.clone();
            let acceptor = acceptor.clone();
            self.shared.tasks.spawn(async move {
                relay.handle_tcp(tcp, ip, acceptor).await;
                relay.shared.per_ip.release(ip);
                relay.shared.active.fetch_sub(1, Ordering::SeqCst);
            });
        }
    }

    async fn handle_tcp(&self, tcp: TcpStream, ip: IpAddr, acceptor: TlsAcceptor) {
        let _ = tcp.set_nodelay(true);
        let deadline = Instant::now() + self.shared.cfg.handshake_timeout;
        let tls = match tokio::time::timeout_at(deadline.into(), acceptor.accept(tcp)).await {
            Ok(Ok(t)) => t,
            _ => return,
        };
        let (alpn_ok, exporter) = {
            let conn = tls.get_ref().1;
            let ok = conn.alpn_protocol() == Some(ALPN);
            let mut ex = [0u8; EXPORTER_LEN];
            let ex_ok = conn
                .export_keying_material(&mut ex, EXPORTER_LABEL, None)
                .is_ok();
            (ok && ex_ok, ex)
        };
        if !alpn_ok {
            // Wrong / missing ALPN (incl. completed acme-tls/1 validations):
            // close without sending application data.
            let mut tls = tls;
            let _ = tls.shutdown().await;
            return;
        }
        debug!(peer = %self.tag(ip), "session open");
        self.session(tls, deadline, exporter).await;
        debug!(peer = %self.tag(ip), "session closed");
    }

    async fn session<S>(&self, stream: S, deadline: Instant, exporter: [u8; EXPORTER_LEN])
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let s = &self.shared;
        let (mut rd, mut wr) = tokio::io::split(stream);
        let (tx, mut rx) = mpsc::channel::<ServerMsg>(64);
        let writer = tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                let bytes = m.encode();
                let w = write_frame(&mut wr, &bytes);
                if !matches!(
                    tokio::time::timeout(Duration::from_secs(10), w).await,
                    Ok(Ok(()))
                ) {
                    break;
                }
            }
            let _ = wr.shutdown().await;
        });

        // ---- handshake: Hello → Challenge → Auth → (fixed delay) → Registered
        let hs = async {
            let f1 = read_frame(&mut rd).await?;
            let hello = ClientMsg::decode(&f1).ok();
            let nonce: [u8; 32] = random();
            let _ = tx.send(ServerMsg::Challenge { nonce }).await;
            let f2 = read_frame(&mut rd).await?;
            let t_auth = Instant::now();
            let auth = ClientMsg::decode(&f2).ok();
            Ok::<_, ProtoError>((hello, nonce, auth, t_auth))
        };
        let Ok(Ok((hello, nonce, auth, t_auth))) =
            tokio::time::timeout_at(deadline.into(), hs).await
        else {
            drop(tx);
            let _ = writer.await;
            return;
        };

        let claim = match (hello, auth) {
            (
                Some(ClientMsg::Hello {
                    role,
                    rendezvous_id,
                    public_key,
                }),
                Some(ClientMsg::Auth { signature }),
            ) => {
                let sig_ok = verify_auth(
                    &public_key,
                    &nonce,
                    &rendezvous_id,
                    role,
                    &exporter,
                    &signature,
                );
                Some((role, rendezvous_id, public_key, sig_ok))
            }
            _ => None,
        };

        tokio::time::sleep_until((t_auth + s.cfg.auth_delay).into()).await;

        let conn_id = s.next_conn.fetch_add(1, Ordering::Relaxed);
        let kick = s.shutdown.child_token();
        let binding = claim.and_then(|(role, rid, pk, sig_ok)| {
            self.register(role, rid, pk, sig_ok, conn_id, &tx, &kick)
        });
        let mut cred_user = self.issue_credential(binding, &tx, None);
        if let Some(b) = binding {
            self.announce_online(&b);
        }

        // ---- main loop
        let mut bucket = (s.cfg.msg_burst, Instant::now());
        loop {
            let frame = tokio::select! {
                _ = kick.cancelled() => break,
                r = tokio::time::timeout(s.cfg.idle_timeout, read_frame(&mut rd)) => r,
            };
            let Ok(Ok(frame)) = frame else { break };
            let now = Instant::now();
            bucket.0 = (bucket.0 + now.duration_since(bucket.1).as_secs_f64() * s.cfg.msg_rate)
                .min(s.cfg.msg_burst);
            bucket.1 = now;
            if bucket.0 < 1.0 {
                continue; // over rate: drop silently
            }
            bucket.0 -= 1.0;
            let Ok(msg) = ClientMsg::decode(&frame) else {
                break;
            };
            match msg {
                ClientMsg::Ping => {
                    let _ = tx.try_send(ServerMsg::Pong);
                }
                ClientMsg::RefreshStun => {
                    cred_user = self.issue_credential(binding, &tx, Some(cred_user));
                }
                ClientMsg::Forward { blob } => {
                    if let Some(b) = binding {
                        self.forward(&b, blob);
                    }
                }
                ClientMsg::SetCandidates { blob } => {
                    if let Some(b) = binding {
                        self.set_candidates(&b, blob);
                    }
                }
                ClientMsg::PunchRequest => {
                    if let Some(b) = binding {
                        let mut ids = s.ids.lock().unwrap();
                        if let Some(e) = ids.get_mut(&b.rendezvous_id)
                            && owns(e, &b)
                        {
                            try_punch(&s.cfg, e);
                        }
                    }
                }
                ClientMsg::Hello { .. } | ClientMsg::Auth { .. } => break,
            }
        }

        // ---- teardown
        s.creds.lock().unwrap().remove(&cred_user);
        if let Some(b) = binding {
            self.release(&b);
        }
        drop(tx);
        let _ = writer.await;
    }

    /// Admit a proven peer into its slot. None ⇒ decoy.
    #[allow(clippy::too_many_arguments)]
    fn register(
        &self,
        role: Role,
        rid: [u8; 32],
        pk: [u8; 32],
        sig_ok: bool,
        conn_id: u64,
        tx: &mpsc::Sender<ServerMsg>,
        kick: &CancellationToken,
    ) -> Option<Binding> {
        let s = &self.shared;
        let mut ids = s.ids.lock().unwrap();
        let known_ok = ids.get(&rid).map(|e| bool::from(e.public_key.ct_eq(&pk)));
        let admit = match known_ok {
            Some(pk_ok) => sig_ok & pk_ok,
            None => {
                if sig_ok && ids.len() >= s.cfg.max_ids {
                    let now = Instant::now();
                    let ttl = s.cfg.id_ttl;
                    ids.retain(|_, e| {
                        e.slots.iter().any(Option::is_some)
                            || now.saturating_duration_since(e.idle_since) < ttl
                    });
                }
                sig_ok && ids.len() < s.cfg.max_ids
            }
        };
        if !admit {
            return None;
        }
        let entry = ids.entry(rid).or_insert_with(|| IdEntry {
            public_key: pk,
            slots: [None, None],
            idle_since: Instant::now(),
            punch_rounds: 0,
            last_punch: None,
        });
        let slot = Slot {
            conn_id,
            tx: tx.clone(),
            kick: kick.clone(),
            public: None,
            candidates: Vec::new(),
        };
        // A re-registering peer (reconnect, new device) replaces the old one.
        if let Some(old) = entry.slots[role.index()].replace(slot) {
            old.kick.cancel();
        }
        entry.punch_rounds = 0;
        entry.last_punch = None;
        Some(Binding {
            rendezvous_id: rid,
            role,
            conn_id,
        })
    }

    fn issue_credential(
        &self,
        bind: Option<Binding>,
        tx: &mpsc::Sender<ServerMsg>,
        previous: Option<String>,
    ) -> String {
        let s = &self.shared;
        let username = hex(&random::<16>());
        let password = hex(&random::<16>());
        {
            let mut creds = s.creds.lock().unwrap();
            if let Some(p) = previous {
                creds.remove(&p);
            }
            creds.insert(
                username.clone(),
                Cred {
                    password: password.clone(),
                    expires: Instant::now() + s.cfg.stun_credential_ttl,
                    bind,
                },
            );
        }
        let _ = tx.try_send(ServerMsg::Registered {
            stun_username: username.clone(),
            stun_password: password,
            ttl_secs: s.cfg.stun_credential_ttl.as_secs() as u32,
            stun_port: s.stun_port,
        });
        username
    }

    fn announce_online(&self, b: &Binding) {
        let ids = self.shared.ids.lock().unwrap();
        let Some(e) = ids.get(&b.rendezvous_id) else {
            return;
        };
        if let (Some(me), Some(peer)) = (&e.slots[b.role.index()], &e.slots[b.role.other().index()])
        {
            let _ = me.tx.try_send(ServerMsg::PeerOnline);
            let _ = peer.tx.try_send(ServerMsg::PeerOnline);
        }
    }

    fn forward(&self, b: &Binding, blob: Vec<u8>) {
        // Peers only ever exchange sealed blobs; anything shorter than an
        // empty sealed box can't be one.
        if blob.len() < SEALED_OVERHEAD || blob.len() > MAX_BLOB {
            return;
        }
        let ids = self.shared.ids.lock().unwrap();
        let Some(e) = ids.get(&b.rendezvous_id) else {
            return;
        };
        if !owns(e, b) {
            return;
        }
        if let Some(peer) = &e.slots[b.role.other().index()] {
            let _ = peer.tx.try_send(ServerMsg::Forwarded { blob });
        }
    }

    fn set_candidates(&self, b: &Binding, blob: Vec<u8>) {
        if blob.len() > MAX_BLOB || (!blob.is_empty() && blob.len() < SEALED_OVERHEAD) {
            return;
        }
        let mut ids = self.shared.ids.lock().unwrap();
        let Some(e) = ids.get_mut(&b.rendezvous_id) else {
            return;
        };
        if !owns(e, b) {
            return;
        }
        if let Some(me) = e.slots[b.role.index()].as_mut() {
            me.candidates = blob;
        }
        try_punch(&self.shared.cfg, e);
    }

    fn release(&self, b: &Binding) {
        let mut ids = self.shared.ids.lock().unwrap();
        let Some(e) = ids.get_mut(&b.rendezvous_id) else {
            return;
        };
        if !owns(e, b) {
            return; // already replaced by a newer session
        }
        e.slots[b.role.index()] = None;
        e.idle_since = Instant::now();
        if let Some(peer) = &e.slots[b.role.other().index()] {
            let _ = peer.tx.try_send(ServerMsg::PeerOffline);
        }
    }

    // ---- STUN -------------------------------------------------------

    pub async fn serve_stun(&self, socket: UdpSocket) {
        let s = &self.shared;
        let mut buf = [0u8; stun::MAX_PACKET + 1];
        loop {
            let r = tokio::select! {
                _ = s.shutdown.cancelled() => break,
                r = socket.recv_from(&mut buf) => r,
            };
            let Ok((n, src)) = r else { continue };
            if !s.stun_limiter.allow(src.ip()) {
                continue;
            }
            let Some(req) = stun::parse_request(&buf[..n]) else {
                continue;
            };
            let found = {
                let creds = s.creds.lock().unwrap();
                creds
                    .get(req.username)
                    .filter(|c| c.expires > Instant::now())
                    .map(|c| (c.password.clone(), c.bind))
            };
            let Some((password, bind)) = found else {
                continue;
            };
            if !req.verify(password.as_bytes()) {
                continue;
            }
            if let Some(b) = bind {
                let mut ids = s.ids.lock().unwrap();
                if let Some(e) = ids.get_mut(&b.rendezvous_id)
                    && owns(e, &b)
                {
                    let me = e.slots[b.role.index()].as_mut().expect("owned");
                    if me.public != Some(src) {
                        me.public = Some(src);
                        try_punch(&s.cfg, e);
                    }
                }
            }
            let resp = stun::build_response(&req.txid, src, password.as_bytes());
            if let Err(e) = socket.send_to(&resp, src).await {
                debug!("stun send: {e}");
            }
        }
    }
}

fn owns(e: &IdEntry, b: &Binding) -> bool {
    e.slots[b.role.index()]
        .as_ref()
        .is_some_and(|s| s.conn_id == b.conn_id)
}

/// Send both peers a `PunchNow` if both are online with STUN-learned
/// endpoints, within the round budget and minimum spacing.
fn try_punch(cfg: &RelayConfig, e: &mut IdEntry) {
    let (Some(a), Some(b)) = (&e.slots[0], &e.slots[1]) else {
        return;
    };
    let (Some(a_pub), Some(b_pub)) = (a.public, b.public) else {
        return;
    };
    if e.punch_rounds >= cfg.max_punch_rounds {
        return;
    }
    if e.last_punch
        .is_some_and(|t| t.elapsed() < cfg.punch_min_interval)
    {
        return;
    }
    e.punch_rounds += 1;
    e.last_punch = Some(Instant::now());
    let nonce: [u8; 16] = random();
    let start_at_ms = unix_ms() + cfg.punch_lead.as_millis() as u64;
    let msg = |peer_public, peer_candidates: &Vec<u8>| ServerMsg::PunchNow {
        peer_public,
        start_at_ms,
        nonce,
        attempt: e.punch_rounds,
        peer_candidates: peer_candidates.clone(),
    };
    let to_a = msg(b_pub, &b.candidates);
    let to_b = msg(a_pub, &a.candidates);
    if a.tx.try_send(to_a).is_err() || b.tx.try_send(to_b).is_err() {
        warn!("punch notification dropped (slow peer)");
    }
    info!(round = e.punch_rounds, "punch coordinated");
}
