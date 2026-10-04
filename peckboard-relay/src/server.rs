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
//! Pong for Ping, the same unpaired-session lifetime — and nothing else,
//! ever. Only framing violations (an oversize length prefix, EOF) close
//! early, identically in both modes.
//!
//! Relay fallback (protocol v2, see [`crate::proto::ALPN_V2`]): when the
//! two peers of an id cannot punch a direct path, they send their (already
//! end-to-end encrypted QUIC) datagrams as [`ClientMsg::Data`] over this
//! same authenticated session and the relay forwards them verbatim to the
//! other peer's session. Nothing about them is stored or logged — only
//! aggregate counters ([`Relay::relay_stats`]). Bandwidth is capped per id,
//! per source IP and globally; over-limit datagrams are dropped (QUIC
//! congestion control backs off, exactly as on a lossy path).
//!
//! Denial of service: every per-address limit keys on the canonical IPv4
//! address or the IPv6 /64, /56 and /48 together ([`crate::limits`]). A
//! full connection table evicts the oldest decoy / handshaking session,
//! then the oldest unpaired one, never a paired one; unpaired sessions get
//! a bounded lifetime. Outbound queues are bounded in bytes per session and
//! relay-wide, and a peer that reads slower than a minimum drain rate is
//! cut off. New ids are budgeted per address; never-paired ids expire after
//! minutes, and a full id table drops the longest-idle id. Relay pair slots
//! are capped per address and shared max-min fairly between addresses.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rand::{Rng, RngCore};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{debug, info, warn};

use crate::keys::{EXPORTER_LABEL, EXPORTER_LEN, SEALED_OVERHEAD, verify_auth};
use crate::limits::{ConnCounter, PrefixCaps, RateLimiter, TokenBucket, canonical_ip, ip_tag};
use crate::proto::{
    ALPN, ALPN_V2, ClientMsg, MAX_BLOB, ProtoError, Role, ServerMsg, read_frame, write_frame,
};
use crate::stun;

/// Default global connection cap. `LimitNOFILE` is 16384; at ~100 KiB of
/// TLS state + bounded queues per session this stays well inside the
/// unit's `MemoryMax`.
pub const DEFAULT_MAX_CONNECTIONS: usize = 8192;
/// Default concurrent connections per IPv4 address. Mobile carrier NAT
/// hands each subscriber a port block of typically 512-4096 ports, so one
/// public IPv4 fronts at most a few hundred subscribers; 256 lets that many
/// Peckboard users share an address at once (one session each), and
/// eviction lets newcomers displace a squatter's idle decoys.
pub const DEFAULT_MAX_CONNECTIONS_PER_IP: usize = 256;
/// Default per IPv6 /64 (one subscriber); /56 and /48 get ×2 and ×4.
pub const DEFAULT_MAX_CONNECTIONS_PER_V6_64: usize = 32;
/// Bookkeeping charged per queued frame on top of its bytes.
pub const QUEUE_ITEM_OVERHEAD: usize = 64;
/// Signaling frames queued per session (count backstop to the byte cap).
const SIGNAL_QUEUE_LEN: usize = 64;
/// Sessions evicted but still tearing down may exceed `max_connections`
/// by this share (and at least [`EVICTION_SLACK_MIN`]); beyond that, refuse.
const EVICTION_SLACK_DIVISOR: usize = 8;
const EVICTION_SLACK_MIN: usize = 16;
/// Teardown flushes what is queued for at most this long.
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(2);
/// A pair refused a relay slot retries acquisition at most this often.
const RELAY_RETRY_INTERVAL: Duration = Duration::from_secs(1);
/// Idle-id records may outnumber live ids by this factor (+ slack) before
/// stale ones are compacted away.
const IDLE_QUEUE_SLACK: usize = 1024;

#[derive(Clone, Debug)]
pub struct RelayConfig {
    pub max_connections: usize,
    /// Concurrent connections per IPv4 address.
    pub max_connections_per_ip: usize,
    /// Concurrent connections per IPv6 /64 (/56 ×2, /48 ×4).
    pub max_connections_per_v6_64: usize,
    /// New TCP connections per second per IP (v6: /64, /56 ×2, /48 ×4) and
    /// burst.
    pub conn_rate_per_ip: f64,
    pub conn_burst_per_ip: f64,
    pub conn_rate_global: f64,
    pub conn_burst_global: f64,
    /// STUN packets per second per IP and burst, checked before anything
    /// else.
    pub stun_rate_per_ip: f64,
    pub stun_burst_per_ip: f64,
    /// STUN requests per second per credential and burst.
    pub stun_rate_per_cred: f64,
    pub stun_burst_per_cred: f64,
    /// Global STUN ceiling, charged only for well-formed requests naming a
    /// live credential.
    pub stun_rate_global: f64,
    pub stun_burst_global: f64,
    /// Signaling messages per second per session and burst.
    pub msg_rate: f64,
    pub msg_burst: f64,
    /// Cap on remembered rendezvous ids; when full, the longest-idle id
    /// (never-paired first) is dropped for a newcomer.
    pub max_ids: usize,
    /// How long an id that has seen both peers stays remembered with no
    /// peer online.
    pub id_ttl: Duration,
    /// Same, for an id that never had both peers online at once.
    pub id_ttl_unpaired: Duration,
    /// New rendezvous ids one IP may create per second (v6: /64, /56 ×2,
    /// /48 ×4) and burst; over budget, the session is a decoy.
    pub new_id_rate_per_ip: f64,
    pub new_id_burst_per_ip: f64,
    pub new_id_rate_global: f64,
    pub new_id_burst_global: f64,
    /// TLS handshake + Hello + Auth must finish within this.
    pub handshake_timeout: Duration,
    /// Close a session after this long without a frame that counts (Data
    /// frames only count while the peer is online).
    pub idle_timeout: Duration,
    /// A session without an online peer (decoys alike) is closed after this
    /// long unpaired, plus a random jitter up to `unpaired_lifetime_jitter`.
    /// Clients reconnect.
    pub unpaired_max_lifetime: Duration,
    pub unpaired_lifetime_jitter: Duration,
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
    /// Forward v2 `Data` datagrams between the peers of an id (relay
    /// fallback). Off: data frames are dropped; the binary also stops
    /// offering protocol v2 then, so clients don't try.
    pub relay_enabled: bool,
    /// Relayed bytes per second per rendezvous id (both directions) and
    /// burst.
    pub relay_rate_per_id: f64,
    pub relay_burst_per_id: f64,
    /// Relayed bytes per second per sending IP (v6 /64) and burst.
    pub relay_rate_per_ip: f64,
    pub relay_burst_per_ip: f64,
    /// Relayed bytes per second across all ids and burst.
    pub relay_rate_global: f64,
    pub relay_burst_global: f64,
    /// Ids allowed to relay at the same time. When full, a new pair takes
    /// the slot of a pair whose addresses hold strictly more slots than its
    /// own will; otherwise its datagrams are dropped until one goes idle.
    pub relay_max_pairs: usize,
    /// Relay pair slots one IPv4 address may be party to (each pair counts
    /// once per end).
    pub relay_max_pairs_per_ip: usize,
    /// Same per IPv6 /64 (/56 ×2, /48 ×4).
    pub relay_max_pairs_per_v6_64: usize,
    /// A relaying id that sent nothing for this long frees its pair slot.
    pub relay_idle_timeout: Duration,
    /// Datagrams queued per session towards a slow peer (count backstop to
    /// `queue_data_bytes`).
    pub relay_queue: usize,
    /// Bytes of signaling queued per session before dropping.
    pub queue_signal_bytes: usize,
    /// Bytes of relayed datagrams queued per session before dropping.
    pub queue_data_bytes: usize,
    /// Bytes queued across all sessions before dropping.
    pub queue_global_bytes: usize,
    /// While a session has a backlog, its socket must accept at least this
    /// many bytes per second, averaged over `drain_window`, or it is closed.
    pub min_drain_rate: f64,
    pub drain_window: Duration,
    /// One frame must be written within this.
    pub write_timeout: Duration,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            max_connections: DEFAULT_MAX_CONNECTIONS,
            max_connections_per_ip: DEFAULT_MAX_CONNECTIONS_PER_IP,
            max_connections_per_v6_64: DEFAULT_MAX_CONNECTIONS_PER_V6_64,
            conn_rate_per_ip: 2.0,
            conn_burst_per_ip: 64.0,
            conn_rate_global: 200.0,
            conn_burst_global: 1000.0,
            stun_rate_per_ip: 5.0,
            stun_burst_per_ip: 40.0,
            stun_rate_per_cred: 10.0,
            stun_burst_per_cred: 40.0,
            stun_rate_global: 2000.0,
            stun_burst_global: 5000.0,
            msg_rate: 10.0,
            msg_burst: 40.0,
            max_ids: 100_000,
            id_ttl: Duration::from_secs(24 * 3600),
            id_ttl_unpaired: Duration::from_secs(10 * 60),
            new_id_rate_per_ip: 256.0 / 3600.0,
            new_id_burst_per_ip: 256.0,
            new_id_rate_global: 100.0,
            new_id_burst_global: 2000.0,
            handshake_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(90),
            unpaired_max_lifetime: Duration::from_secs(30 * 60),
            unpaired_lifetime_jitter: Duration::from_secs(10 * 60),
            auth_delay: Duration::from_millis(150),
            stun_credential_ttl: Duration::from_secs(600),
            punch_lead: Duration::from_millis(500),
            max_punch_rounds: 8,
            punch_min_interval: Duration::from_secs(1),
            log_full_ips: false,
            relay_enabled: true,
            relay_rate_per_id: 512.0 * 1024.0,
            relay_burst_per_id: 2.0 * 1024.0 * 1024.0,
            relay_rate_per_ip: 1024.0 * 1024.0,
            relay_burst_per_ip: 4.0 * 1024.0 * 1024.0,
            relay_rate_global: 50.0 * 1024.0 * 1024.0,
            relay_burst_global: 100.0 * 1024.0 * 1024.0,
            relay_max_pairs: 1000,
            relay_max_pairs_per_ip: 32,
            relay_max_pairs_per_v6_64: 8,
            relay_idle_timeout: Duration::from_secs(60),
            relay_queue: 128,
            queue_signal_bytes: 32 * 1024,
            queue_data_bytes: 96 * 1024,
            queue_global_bytes: 256 * 1024 * 1024,
            min_drain_rate: 4096.0,
            drain_window: Duration::from_secs(15),
            write_timeout: Duration::from_secs(10),
        }
    }
}

/// Aggregate relay-data counters since start (no per-id data, ever).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RelayStats {
    pub packets: u64,
    pub bytes: u64,
    /// Dropped by a per-id / per-IP / global bandwidth cap or the pair cap.
    pub dropped_limit: u64,
    /// Dropped because the peer's queue was full.
    pub dropped_queue: u64,
    /// Dropped because the peer was offline or speaks only v1.
    pub dropped_no_peer: u64,
    pub active_pairs: usize,
}

#[derive(Default)]
struct StatCounters {
    packets: AtomicU64,
    bytes: AtomicU64,
    dropped_limit: AtomicU64,
    dropped_queue: AtomicU64,
    dropped_no_peer: AtomicU64,
}

type DataTap = Box<dyn Fn(&[u8]) + Send + Sync>;

/// Handshaking, or a decoy.
const CLASS_UNAUTH: u8 = 0;
/// Registered, peer offline.
const CLASS_ALONE: u8 = 1;
/// Registered with the peer online.
const CLASS_PAIRED: u8 = 2;

/// One live connection, from accept to teardown.
struct SessionMeta {
    conn_id: u64,
    /// Canonical client address.
    ip: IpAddr,
    started: Instant,
    /// Since when (ms after [`Shared::epoch`]) the session has had no
    /// online peer.
    alone_since_ms: AtomicU64,
    /// Unpaired lifetime including this session's jitter.
    lifetime: Duration,
    class: AtomicU8,
    /// Ends the session (replacement, eviction, lifetime, writer failure,
    /// shutdown).
    kick: CancellationToken,
}

impl SessionMeta {
    fn class(&self) -> u8 {
        self.class.load(Ordering::Relaxed)
    }

    fn set_class(&self, c: u8) {
        self.class.store(c, Ordering::Relaxed);
    }
}

fn item_cost(len: usize) -> usize {
    len + QUEUE_ITEM_OVERHEAD
}

/// Byte budget for frames queued towards one session's socket, plus the
/// relay-wide total. Reserved on enqueue, released once written or dropped.
struct QueueBudget {
    signal: AtomicUsize,
    data: AtomicUsize,
    total: Arc<AtomicUsize>,
    signal_cap: usize,
    data_cap: usize,
    total_cap: usize,
}

impl QueueBudget {
    fn counter(&self, data: bool) -> (&AtomicUsize, usize) {
        if data {
            (&self.data, self.data_cap)
        } else {
            (&self.signal, self.signal_cap)
        }
    }

    fn reserve(&self, data: bool, n: usize) -> bool {
        let (mine, cap) = self.counter(data);
        if mine.fetch_add(n, Ordering::SeqCst) + n > cap {
            mine.fetch_sub(n, Ordering::SeqCst);
            return false;
        }
        if self.total.fetch_add(n, Ordering::SeqCst) + n > self.total_cap {
            self.total.fetch_sub(n, Ordering::SeqCst);
            mine.fetch_sub(n, Ordering::SeqCst);
            return false;
        }
        true
    }

    fn release(&self, data: bool, n: usize) {
        self.counter(data).0.fetch_sub(n, Ordering::SeqCst);
        self.total.fetch_sub(n, Ordering::SeqCst);
    }

    fn queued(&self) -> usize {
        self.signal.load(Ordering::SeqCst) + self.data.load(Ordering::SeqCst)
    }
}

/// Sending side of one session's outbound queues. Never blocks: a frame
/// over the byte budget (or count backstop) is dropped.
#[derive(Clone)]
struct Outbox {
    signal: mpsc::Sender<Vec<u8>>,
    /// v2 sessions only: datagrams for this peer (separate from `signal` so
    /// a full data queue never delays signaling).
    data: Option<mpsc::Sender<Vec<u8>>>,
    budget: Arc<QueueBudget>,
}

impl Outbox {
    fn send(&self, m: &ServerMsg) -> bool {
        Self::push(&self.signal, &self.budget, false, m.encode())
    }

    fn send_data(&self, packet: Vec<u8>) -> bool {
        match &self.data {
            Some(tx) => Self::push(tx, &self.budget, true, packet),
            None => false,
        }
    }

    fn push(tx: &mpsc::Sender<Vec<u8>>, budget: &QueueBudget, data: bool, b: Vec<u8>) -> bool {
        let n = item_cost(b.len());
        if !budget.reserve(data, n) {
            return false;
        }
        if tx.try_send(b).is_err() {
            budget.release(data, n);
            return false;
        }
        true
    }
}

/// Receiving side, owned by the writer task. Dropping it (normal exit or
/// abort) returns everything still queued to the budgets.
struct WriterRx {
    signal: mpsc::Receiver<Vec<u8>>,
    data: mpsc::Receiver<Vec<u8>>,
    budget: Arc<QueueBudget>,
    in_flight: Option<(bool, usize)>,
}

impl Drop for WriterRx {
    fn drop(&mut self) {
        if let Some((d, n)) = self.in_flight.take() {
            self.budget.release(d, n);
        }
        self.signal.close();
        self.data.close();
        while let Ok(b) = self.signal.try_recv() {
            self.budget.release(false, item_cost(b.len()));
        }
        while let Ok(p) = self.data.try_recv() {
            self.budget.release(true, item_cost(p.len()));
        }
    }
}

#[derive(Clone, Copy)]
struct Pace {
    min_rate: f64,
    window: Duration,
    write_timeout: Duration,
}

struct Slot {
    conn_id: u64,
    ip: IpAddr,
    meta: Arc<SessionMeta>,
    out: Outbox,
    public: Option<SocketAddr>,
    candidates: Vec<u8>,
}

/// An id's relay pair slot.
struct RelayHold {
    started: Instant,
    /// Last datagram relayed.
    last: Instant,
    /// Both ends' addresses, as counted in [`Shared::relay_ips`].
    ips: [IpAddr; 2],
}

struct IdEntry {
    public_key: [u8; 32],
    slots: [Option<Slot>; 2],
    /// Matches the id's current idle record; 0 while a peer is online.
    idle_gen: u64,
    /// Both peers were online at once at some point.
    ever_paired: bool,
    punch_rounds: u8,
    last_punch: Option<Instant>,
    relay_bucket: TokenBucket,
    relay: Option<RelayHold>,
    relay_denied: Option<Instant>,
}

struct IdleRec {
    since: Instant,
    rid: [u8; 32],
    generation: u64,
}

/// The id table plus O(1)-amortised idle bookkeeping.
#[derive(Default)]
struct Ids {
    map: HashMap<[u8; 32], IdEntry>,
    /// Ids with no peer online, oldest first, by whether they were ever
    /// paired. Lazily invalidated: a record counts only while its id's
    /// `idle_gen` still matches.
    idle_new: VecDeque<IdleRec>,
    idle_paired: VecDeque<IdleRec>,
    next_gen: u64,
    /// Ids holding a relay pair slot.
    relaying: HashSet<[u8; 32]>,
}

impl Ids {
    fn is_live(map: &HashMap<[u8; 32], IdEntry>, r: &IdleRec) -> bool {
        map.get(&r.rid).is_some_and(|e| e.idle_gen == r.generation)
    }

    fn queue(&mut self, paired: bool) -> &mut VecDeque<IdleRec> {
        if paired {
            &mut self.idle_paired
        } else {
            &mut self.idle_new
        }
    }

    /// The id's last peer just left.
    fn mark_idle(&mut self, rid: [u8; 32], now: Instant) {
        self.next_gen += 1;
        let generation = self.next_gen;
        let Some(e) = self.map.get_mut(&rid) else {
            return;
        };
        e.idle_gen = generation;
        let paired = e.ever_paired;
        let limit = 2 * self.map.len() + IDLE_QUEUE_SLACK;
        let rec = IdleRec {
            since: now,
            rid,
            generation,
        };
        self.queue(paired).push_back(rec);
        if self.queue(paired).len() > limit {
            let Ids {
                map,
                idle_new,
                idle_paired,
                ..
            } = self;
            let q = if paired { idle_paired } else { idle_new };
            q.retain(|r| Self::is_live(map, r));
        }
    }

    fn remove(&mut self, s: &Shared, rid: &[u8; 32]) {
        if let Some(mut e) = self.map.remove(rid) {
            end_relay(s, &mut self.relaying, rid, &mut e);
        }
    }

    /// Drop the longest-idle id, never-paired first. False when every id
    /// has a peer online.
    fn evict_one(&mut self, s: &Shared) -> bool {
        for paired in [false, true] {
            while let Some(r) = self.queue(paired).pop_front() {
                if Self::is_live(&self.map, &r) {
                    self.remove(s, &r.rid);
                    return true;
                }
            }
        }
        false
    }

    /// Drop ids idle past their TTL (only the expired front of each queue
    /// is touched).
    fn expire(&mut self, s: &Shared, now: Instant) {
        for (paired, ttl) in [(false, s.cfg.id_ttl_unpaired), (true, s.cfg.id_ttl)] {
            while self
                .queue(paired)
                .front()
                .is_some_and(|r| now.saturating_duration_since(r.since) >= ttl)
            {
                let r = self.queue(paired).pop_front().expect("front");
                if Self::is_live(&self.map, &r) {
                    self.remove(s, &r.rid);
                }
            }
        }
    }
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
    bucket: TokenBucket,
}

struct Shared {
    cfg: RelayConfig,
    stun_port: u16,
    ids: Mutex<Ids>,
    creds: Mutex<HashMap<String, Cred>>,
    /// Live sessions by conn id (evicted ones are removed at eviction).
    sessions: Mutex<HashMap<u64, Arc<SessionMeta>>>,
    conn_limiter: RateLimiter,
    /// Per source only; the global STUN ceiling is `stun_global`.
    stun_limiter: RateLimiter,
    stun_global: Mutex<TokenBucket>,
    new_id_limiter: RateLimiter,
    /// Relayed bytes per sending IP + global.
    relay_limiter: RateLimiter,
    /// Pair slots held; changed only under the `ids` lock.
    relay_pairs: AtomicUsize,
    /// Pair slots per address (each pair counts once per end).
    relay_ips: ConnCounter,
    /// Bytes queued towards all sockets.
    queued: Arc<AtomicUsize>,
    stats: StatCounters,
    tap: OnceLock<DataTap>,
    per_ip: ConnCounter,
    /// Session tasks alive, including evicted ones still tearing down.
    active: AtomicUsize,
    next_conn: AtomicU64,
    salt: [u8; 16],
    epoch: Instant,
    shutdown: CancellationToken,
    tasks: TaskTracker,
}

impl Shared {
    fn ms(&self, t: Instant) -> u64 {
        t.saturating_duration_since(self.epoch).as_millis() as u64
    }

    fn at(&self, ms: u64) -> Instant {
        self.epoch + Duration::from_millis(ms)
    }
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

fn take_msg_token(cfg: &RelayConfig, b: &mut (f64, Instant), now: Instant) -> bool {
    b.0 =
        (b.0 + now.saturating_duration_since(b.1).as_secs_f64() * cfg.msg_rate).min(cfg.msg_burst);
    b.1 = now;
    if b.0 < 1.0 {
        return false;
    }
    b.0 -= 1.0;
    true
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
            stun_limiter: RateLimiter::per_ip_only(cfg.stun_rate_per_ip, cfg.stun_burst_per_ip),
            stun_global: Mutex::new(TokenBucket::new(cfg.stun_burst_global)),
            new_id_limiter: RateLimiter::new(
                cfg.new_id_rate_per_ip,
                cfg.new_id_burst_per_ip,
                cfg.new_id_rate_global,
                cfg.new_id_burst_global,
            ),
            relay_limiter: RateLimiter::new(
                cfg.relay_rate_per_ip,
                cfg.relay_burst_per_ip,
                cfg.relay_rate_global,
                cfg.relay_burst_global,
            ),
            relay_pairs: AtomicUsize::new(0),
            relay_ips: ConnCounter::default(),
            queued: Arc::new(AtomicUsize::new(0)),
            stats: StatCounters::default(),
            tap: OnceLock::new(),
            cfg,
            stun_port,
            ids: Mutex::new(Ids::default()),
            creds: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            per_ip: ConnCounter::default(),
            active: AtomicUsize::new(0),
            next_conn: AtomicU64::new(1),
            salt: random(),
            epoch: Instant::now(),
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
        self.shared.ids.lock().unwrap().map.len()
    }

    /// Aggregate relay-data counters (tests / local logs only — never
    /// exposed over the network).
    pub fn relay_stats(&self) -> RelayStats {
        let c = &self.shared.stats;
        RelayStats {
            packets: c.packets.load(Ordering::Relaxed),
            bytes: c.bytes.load(Ordering::Relaxed),
            dropped_limit: c.dropped_limit.load(Ordering::Relaxed),
            dropped_queue: c.dropped_queue.load(Ordering::Relaxed),
            dropped_no_peer: c.dropped_no_peer.load(Ordering::Relaxed),
            active_pairs: self.shared.relay_pairs.load(Ordering::Relaxed),
        }
    }

    /// Test hook: see every datagram the relay forwards, exactly as
    /// forwarded (to assert it is ciphertext). Set at most once.
    #[doc(hidden)]
    pub fn set_data_tap(&self, f: impl Fn(&[u8]) + Send + Sync + 'static) {
        let _ = self.shared.tap.set(Box::new(f));
    }

    fn spawn_housekeeping(&self) {
        let relay = self.clone();
        self.shared.tasks.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            let mut logged = RelayStats::default();
            loop {
                tokio::select! {
                    _ = relay.shared.shutdown.cancelled() => break,
                    _ = tick.tick() => {}
                }
                relay.housekeep(Instant::now());
                let stats = relay.relay_stats();
                if stats != logged {
                    info!(
                        packets = stats.packets,
                        bytes = stats.bytes,
                        dropped_limit = stats.dropped_limit,
                        dropped_queue = stats.dropped_queue,
                        dropped_no_peer = stats.dropped_no_peer,
                        active_pairs = stats.active_pairs,
                        "relay data totals"
                    );
                    logged = stats;
                }
            }
        });
    }

    /// Periodic sweep: limiter tables, expired credentials and ids, idle
    /// relay pairs, and unpaired sessions past their lifetime.
    fn housekeep(&self, now: Instant) {
        let s = &self.shared;
        s.conn_limiter.prune();
        s.stun_limiter.prune();
        s.new_id_limiter.prune();
        s.relay_limiter.prune();
        s.creds.lock().unwrap().retain(|_, c| c.expires > now);
        {
            let mut guard = s.ids.lock().unwrap();
            let ids = &mut *guard;
            let idle = s.cfg.relay_idle_timeout;
            let stale: Vec<[u8; 32]> = ids
                .relaying
                .iter()
                .filter(|r| {
                    ids.map
                        .get(*r)
                        .and_then(|e| e.relay.as_ref())
                        .is_none_or(|h| now.saturating_duration_since(h.last) >= idle)
                })
                .copied()
                .collect();
            for r in stale {
                match ids.map.get_mut(&r) {
                    Some(e) => end_relay(s, &mut ids.relaying, &r, e),
                    None => {
                        ids.relaying.remove(&r);
                    }
                }
            }
            ids.expire(s, now);
        }
        // Decoys and lonely peers alike: indistinguishable, and clients
        // reconnect.
        for m in s.sessions.lock().unwrap().values() {
            let alone_since = s.at(m.alone_since_ms.load(Ordering::Relaxed));
            if m.class() != CLASS_PAIRED && now.saturating_duration_since(alone_since) >= m.lifetime
            {
                m.kick.cancel();
            }
        }
    }

    fn tag(&self, ip: IpAddr) -> String {
        if self.shared.cfg.log_full_ips {
            ip.to_string()
        } else {
            ip_tag(&self.shared.salt, ip)
        }
    }

    // ---- admission ---------------------------------------------------

    /// Take a connection slot for `ip` (canonical), evicting if the table
    /// or the address's prefix is full. None ⇒ refuse.
    fn admit(&self, ip: IpAddr) -> Option<Arc<SessionMeta>> {
        let s = &self.shared;
        let cfg = &s.cfg;
        let mut sessions = s.sessions.lock().unwrap();
        let hard_cap = cfg.max_connections
            + (cfg.max_connections / EVICTION_SLACK_DIVISOR).max(EVICTION_SLACK_MIN);
        if s.active.load(Ordering::SeqCst) >= hard_cap {
            return None;
        }
        let caps = PrefixCaps {
            v4: cfg.max_connections_per_ip,
            v6_64: cfg.max_connections_per_v6_64,
        };
        while let Err(full) = s.per_ip.try_acquire(ip, caps) {
            if !self.evict(&mut sessions, |m| full.contains(m.ip)) {
                return None;
            }
        }
        if sessions.len() >= cfg.max_connections && !self.evict(&mut sessions, |_| true) {
            s.per_ip.release(ip);
            return None;
        }
        let now = Instant::now();
        let jitter = cfg.unpaired_lifetime_jitter.as_millis() as u64;
        let jitter = if jitter == 0 {
            0
        } else {
            rand::thread_rng().gen_range(0..=jitter)
        };
        let meta = Arc::new(SessionMeta {
            conn_id: s.next_conn.fetch_add(1, Ordering::Relaxed),
            ip,
            started: now,
            alone_since_ms: AtomicU64::new(s.ms(now)),
            lifetime: cfg.unpaired_max_lifetime + Duration::from_millis(jitter),
            class: AtomicU8::new(CLASS_UNAUTH),
            kick: s.shutdown.child_token(),
        });
        sessions.insert(meta.conn_id, meta.clone());
        s.active.fetch_add(1, Ordering::SeqCst);
        Some(meta)
    }

    /// Kick the best victim among `pick`ed sessions: decoys and handshakes
    /// first, then unpaired peers, oldest first; never a paired session.
    fn evict(
        &self,
        sessions: &mut HashMap<u64, Arc<SessionMeta>>,
        pick: impl Fn(&SessionMeta) -> bool,
    ) -> bool {
        let victim = sessions
            .values()
            .filter(|m| m.class() != CLASS_PAIRED && pick(m))
            .min_by_key(|m| (m.class(), m.started))
            .map(|m| m.conn_id);
        let Some(id) = victim else {
            return false;
        };
        let m = sessions.remove(&id).expect("listed");
        self.shared.per_ip.release(m.ip);
        m.kick.cancel();
        debug!(peer = %self.tag(m.ip), "session evicted");
        true
    }

    /// A session task ended.
    fn finish(&self, meta: &SessionMeta) {
        let s = &self.shared;
        {
            let mut sessions = s.sessions.lock().unwrap();
            if sessions.remove(&meta.conn_id).is_some() {
                s.per_ip.release(meta.ip);
            }
        }
        s.active.fetch_sub(1, Ordering::SeqCst);
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
            let ip = canonical_ip(peer.ip());
            // Over-limit connections get a bare close: no TLS, no bytes.
            if !self.shared.conn_limiter.allow(ip) {
                continue;
            }
            let Some(meta) = self.admit(ip) else {
                continue;
            };
            let relay = self.clone();
            let acceptor = acceptor.clone();
            self.shared.tasks.spawn(async move {
                relay.handle_tcp(tcp, &meta, acceptor).await;
                relay.finish(&meta);
            });
        }
    }

    async fn handle_tcp(&self, tcp: TcpStream, meta: &Arc<SessionMeta>, acceptor: TlsAcceptor) {
        let _ = tcp.set_nodelay(true);
        let ip = meta.ip;
        let deadline = Instant::now() + self.shared.cfg.handshake_timeout;
        let tls = tokio::select! {
            _ = meta.kick.cancelled() => return,
            r = tokio::time::timeout_at(deadline.into(), acceptor.accept(tcp)) => match r {
                Ok(Ok(t)) => t,
                _ => return,
            },
        };
        let (version, exporter) = {
            let conn = tls.get_ref().1;
            let version = match conn.alpn_protocol() {
                Some(p) if p == ALPN => Some(1u8),
                Some(p) if p == ALPN_V2 => Some(2u8),
                _ => None,
            };
            let mut ex = [0u8; EXPORTER_LEN];
            let ex_ok = conn
                .export_keying_material(&mut ex, EXPORTER_LABEL, None)
                .is_ok();
            (version.filter(|_| ex_ok), ex)
        };
        let Some(version) = version else {
            // Wrong / missing ALPN (incl. completed acme-tls/1 validations):
            // close without sending application data.
            let mut tls = tls;
            let _ = tokio::time::timeout(TEARDOWN_TIMEOUT, tls.shutdown()).await;
            return;
        };
        debug!(peer = %self.tag(ip), version, "session open");
        self.session(tls, meta.clone(), version, deadline, exporter)
            .await;
        debug!(peer = %self.tag(ip), "session closed");
    }

    async fn session<S>(
        &self,
        stream: S,
        meta: Arc<SessionMeta>,
        version: u8,
        deadline: Instant,
        exporter: [u8; EXPORTER_LEN],
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let s = &self.shared;
        let cfg = &s.cfg;
        let (mut rd, wr) = tokio::io::split(stream);
        let (signal_tx, signal_rx) = mpsc::channel::<Vec<u8>>(SIGNAL_QUEUE_LEN);
        let (data_tx, data_rx) = mpsc::channel::<Vec<u8>>(cfg.relay_queue.max(1));
        let budget = Arc::new(QueueBudget {
            signal: AtomicUsize::new(0),
            data: AtomicUsize::new(0),
            total: s.queued.clone(),
            signal_cap: cfg.queue_signal_bytes,
            data_cap: cfg.queue_data_bytes,
            total_cap: cfg.queue_global_bytes,
        });
        let out = Outbox {
            signal: signal_tx,
            data: (version >= 2).then_some(data_tx),
            budget: budget.clone(),
        };
        let pace = Pace {
            min_rate: cfg.min_drain_rate,
            window: cfg.drain_window,
            write_timeout: cfg.write_timeout,
        };
        let rx = WriterRx {
            signal: signal_rx,
            data: data_rx,
            budget,
            in_flight: None,
        };
        let kick = meta.kick.clone();
        let writer = tokio::spawn(write_loop(wr, rx, pace, kick.clone()));

        // ---- handshake: Hello → Challenge → Auth → (fixed delay) → Registered
        let hs = async {
            let f1 = read_frame(&mut rd).await?;
            let hello = ClientMsg::decode(&f1).ok();
            let nonce: [u8; 32] = random();
            let _ = out.send(&ServerMsg::Challenge { nonce });
            let f2 = read_frame(&mut rd).await?;
            let t_auth = Instant::now();
            let auth = ClientMsg::decode(&f2).ok();
            Ok::<_, ProtoError>((hello, nonce, auth, t_auth))
        };
        let hs = tokio::select! {
            _ = kick.cancelled() => None,
            r = tokio::time::timeout_at(deadline.into(), hs) => r.ok().and_then(Result::ok),
        };
        let Some((hello, nonce, auth, t_auth)) = hs else {
            finish_writer(out, writer).await;
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

        let kicked = tokio::select! {
            _ = kick.cancelled() => true,
            _ = tokio::time::sleep_until((t_auth + cfg.auth_delay).into()) => false,
        };
        if kicked {
            finish_writer(out, writer).await;
            return;
        }

        let binding = claim
            .and_then(|(role, rid, pk, sig_ok)| self.register(role, rid, pk, sig_ok, &meta, &out));
        let mut cred_user = self.issue_credential(binding, &out, None);
        if let Some(b) = binding {
            self.announce_online(&b);
        }

        // ---- main loop
        let mut bucket = (cfg.msg_burst, Instant::now());
        let mut alive_until = Instant::now() + cfg.idle_timeout;
        loop {
            let frame = tokio::select! {
                _ = kick.cancelled() => break,
                r = tokio::time::timeout_at(alive_until.into(), read_frame(&mut rd)) => r,
            };
            let Ok(Ok(frame)) = frame else { break };
            let now = Instant::now();
            // Datagrams have their own (byte) limits, not the message rate.
            if version >= 2 && frame.first() == Some(&0x10) {
                let Ok(ClientMsg::Data { packet }) = ClientMsg::decode(&frame) else {
                    break;
                };
                match binding {
                    Some(b) if meta.class() == CLASS_PAIRED => {
                        self.relay_data(&b, packet);
                        alive_until = now + cfg.idle_timeout;
                    }
                    // No online peer (decoys alike): nothing to relay to.
                    // Charged like signaling, and never keeps the session
                    // alive.
                    _ => {
                        s.stats.dropped_no_peer.fetch_add(1, Ordering::Relaxed);
                        let _ = take_msg_token(cfg, &mut bucket, now);
                    }
                }
                continue;
            }
            if !take_msg_token(cfg, &mut bucket, now) {
                continue; // over rate: drop silently
            }
            alive_until = now + cfg.idle_timeout;
            let Ok(msg) = ClientMsg::decode(&frame) else {
                break;
            };
            match msg {
                ClientMsg::Ping => {
                    let _ = out.send(&ServerMsg::Pong);
                }
                ClientMsg::RefreshStun => {
                    cred_user = self.issue_credential(binding, &out, Some(cred_user));
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
                        if let Some(e) = ids.map.get_mut(&b.rendezvous_id)
                            && owns(e, &b)
                        {
                            try_punch(cfg, e);
                        }
                    }
                }
                // A v1 session sending v2 frames, or a second handshake.
                ClientMsg::Data { .. } | ClientMsg::Hello { .. } | ClientMsg::Auth { .. } => break,
            }
        }

        // ---- teardown
        s.creds.lock().unwrap().remove(&cred_user);
        if let Some(b) = binding {
            self.release(&b);
        }
        finish_writer(out, writer).await;
    }

    /// Admit a proven peer into its slot. None ⇒ decoy.
    fn register(
        &self,
        role: Role,
        rid: [u8; 32],
        pk: [u8; 32],
        sig_ok: bool,
        meta: &Arc<SessionMeta>,
        out: &Outbox,
    ) -> Option<Binding> {
        let s = &self.shared;
        let mut guard = s.ids.lock().unwrap();
        let ids = &mut *guard;
        let known_ok = ids
            .map
            .get(&rid)
            .map(|e| bool::from(e.public_key.ct_eq(&pk)));
        let admit = match known_ok {
            Some(pk_ok) => sig_ok & pk_ok,
            // A new id: within this address's creation budget, and room in
            // the table (dropping the longest-idle id if full).
            None => {
                sig_ok
                    && s.new_id_limiter.allow(meta.ip)
                    && (ids.map.len() < s.cfg.max_ids || ids.evict_one(s))
            }
        };
        if !admit {
            return None;
        }
        let entry = ids.map.entry(rid).or_insert_with(|| IdEntry {
            public_key: pk,
            slots: [None, None],
            idle_gen: 0,
            ever_paired: false,
            punch_rounds: 0,
            last_punch: None,
            relay_bucket: TokenBucket::new(s.cfg.relay_burst_per_id),
            relay: None,
            relay_denied: None,
        });
        entry.idle_gen = 0;
        let slot = Slot {
            conn_id: meta.conn_id,
            ip: meta.ip,
            meta: meta.clone(),
            out: out.clone(),
            public: None,
            candidates: Vec::new(),
        };
        // A re-registering peer (reconnect, new device) replaces the old one.
        if let Some(old) = entry.slots[role.index()].replace(slot) {
            old.meta.kick.cancel();
        }
        entry.punch_rounds = 0;
        entry.last_punch = None;
        match &entry.slots[role.other().index()] {
            Some(peer) => {
                entry.ever_paired = true;
                peer.meta.set_class(CLASS_PAIRED);
                meta.set_class(CLASS_PAIRED);
            }
            None => meta.set_class(CLASS_ALONE),
        }
        Some(Binding {
            rendezvous_id: rid,
            role,
            conn_id: meta.conn_id,
        })
    }

    fn issue_credential(
        &self,
        bind: Option<Binding>,
        out: &Outbox,
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
                    bucket: TokenBucket::new(s.cfg.stun_burst_per_cred),
                },
            );
        }
        let _ = out.send(&ServerMsg::Registered {
            stun_username: username.clone(),
            stun_password: password,
            ttl_secs: s.cfg.stun_credential_ttl.as_secs() as u32,
            stun_port: s.stun_port,
        });
        username
    }

    fn announce_online(&self, b: &Binding) {
        let ids = self.shared.ids.lock().unwrap();
        let Some(e) = ids.map.get(&b.rendezvous_id) else {
            return;
        };
        if let (Some(me), Some(peer)) = (&e.slots[b.role.index()], &e.slots[b.role.other().index()])
        {
            let _ = me.out.send(&ServerMsg::PeerOnline);
            let _ = peer.out.send(&ServerMsg::PeerOnline);
        }
    }

    fn forward(&self, b: &Binding, blob: Vec<u8>) {
        // Peers only ever exchange sealed blobs; anything shorter than an
        // empty sealed box can't be one.
        if blob.len() < SEALED_OVERHEAD || blob.len() > MAX_BLOB {
            return;
        }
        let ids = self.shared.ids.lock().unwrap();
        let Some(e) = ids.map.get(&b.rendezvous_id) else {
            return;
        };
        if !owns(e, b) {
            return;
        }
        if let Some(peer) = &e.slots[b.role.other().index()] {
            let _ = peer.out.send(&ServerMsg::Forwarded { blob });
        }
    }

    /// Forward one v2 datagram to the other peer, within the bandwidth caps.
    /// Opaque: never inspected, stored or logged.
    fn relay_data(&self, b: &Binding, packet: Vec<u8>) {
        let s = &self.shared;
        let st = &s.stats;
        if !s.cfg.relay_enabled {
            st.dropped_no_peer.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let len = packet.len();
        let now = Instant::now();
        let (peer_out, my_ip) = {
            let mut ids = s.ids.lock().unwrap();
            let Some(e) = ids.map.get(&b.rendezvous_id) else {
                return;
            };
            if !owns(e, b) {
                return;
            }
            let my_ip = e.slots[b.role.index()].as_ref().expect("owned").ip;
            // Both ends online — each slot holds a verified signature — and
            // the peer speaks v2.
            let Some(peer) = e.slots[b.role.other().index()]
                .as_ref()
                .filter(|p| p.out.data.is_some())
            else {
                st.dropped_no_peer.fetch_add(1, Ordering::Relaxed);
                return;
            };
            let (peer_out, ips, held) = (peer.out.clone(), [my_ip, peer.ip], e.relay.is_some());
            if !held && !self.acquire_relay(&mut ids, &b.rendezvous_id, ips, now) {
                st.dropped_limit.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let e = ids.map.get_mut(&b.rendezvous_id).expect("present");
            if let Some(h) = e.relay.as_mut() {
                h.last = now;
            }
            if !e.relay_bucket.take(
                s.cfg.relay_rate_per_id,
                s.cfg.relay_burst_per_id,
                len as f64,
            ) {
                st.dropped_limit.fetch_add(1, Ordering::Relaxed);
                return;
            }
            (peer_out, my_ip)
        };
        if !s.relay_limiter.allow_cost(my_ip, len as f64) {
            st.dropped_limit.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if let Some(tap) = s.tap.get() {
            tap(&packet);
        }
        if peer_out.send_data(packet) {
            st.packets.fetch_add(1, Ordering::Relaxed);
            st.bytes.fetch_add(len as u64, Ordering::Relaxed);
        } else {
            st.dropped_queue.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Give `rid` a relay pair slot (its ends at `ips`). Refusals are
    /// remembered for [`RELAY_RETRY_INTERVAL`] so a refused pair can't make
    /// every datagram rescan the table.
    fn acquire_relay(&self, ids: &mut Ids, rid: &[u8; 32], ips: [IpAddr; 2], now: Instant) -> bool {
        let recently_denied = ids
            .map
            .get(rid)
            .and_then(|e| e.relay_denied)
            .is_some_and(|t| now.saturating_duration_since(t) < RELAY_RETRY_INTERVAL);
        if recently_denied {
            return false;
        }
        let ok = self.take_relay_slot(ids, rid, ips);
        if let Some(e) = ids.map.get_mut(rid) {
            if ok {
                e.relay = Some(RelayHold {
                    started: now,
                    last: now,
                    ips,
                });
                e.relay_denied = None;
            } else {
                e.relay_denied = Some(now);
            }
        }
        ok
    }

    fn take_relay_slot(&self, ids: &mut Ids, rid: &[u8; 32], ips: [IpAddr; 2]) -> bool {
        let s = &self.shared;
        let caps = PrefixCaps {
            v4: s.cfg.relay_max_pairs_per_ip,
            v6_64: s.cfg.relay_max_pairs_per_v6_64,
        };
        if s.relay_ips.try_acquire(ips[0], caps).is_err() {
            return false;
        }
        if s.relay_ips.try_acquire(ips[1], caps).is_err() {
            s.relay_ips.release(ips[0]);
            return false;
        }
        if s.relay_pairs.load(Ordering::SeqCst) >= s.cfg.relay_max_pairs {
            // Full: take the slot of the pair whose busiest address holds
            // strictly more slots than ours now does (max-min fair between
            // addresses; the loser can't take it straight back).
            let load = |ips: &[IpAddr; 2]| {
                ips.iter()
                    .map(|ip| s.relay_ips.count(*ip))
                    .max()
                    .unwrap_or(0)
            };
            let mine = load(&ips);
            let victim = ids
                .relaying
                .iter()
                .filter_map(|r| {
                    let h = ids.map.get(r)?.relay.as_ref()?;
                    let l = load(&h.ips);
                    (l > mine).then_some((l, Reverse(h.started), *r))
                })
                .max()
                .map(|v| v.2);
            let Some(v) = victim else {
                s.relay_ips.release(ips[0]);
                s.relay_ips.release(ips[1]);
                return false;
            };
            let Ids { map, relaying, .. } = &mut *ids;
            if let Some(e) = map.get_mut(&v) {
                end_relay(s, relaying, &v, e);
            }
            debug!("relay pair preempted");
        }
        s.relay_pairs.fetch_add(1, Ordering::SeqCst);
        ids.relaying.insert(*rid);
        debug!("relay pair started");
        true
    }

    fn set_candidates(&self, b: &Binding, blob: Vec<u8>) {
        if blob.len() > MAX_BLOB || (!blob.is_empty() && blob.len() < SEALED_OVERHEAD) {
            return;
        }
        let mut ids = self.shared.ids.lock().unwrap();
        let Some(e) = ids.map.get_mut(&b.rendezvous_id) else {
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
        let s = &self.shared;
        let now = Instant::now();
        let mut guard = s.ids.lock().unwrap();
        let ids = &mut *guard;
        let Some(e) = ids.map.get_mut(&b.rendezvous_id) else {
            return;
        };
        if !owns(e, b) {
            return; // already replaced by a newer session
        }
        e.slots[b.role.index()] = None;
        end_relay(s, &mut ids.relaying, &b.rendezvous_id, e);
        let peer = e.slots[b.role.other().index()]
            .as_ref()
            .map(|p| (p.meta.clone(), p.out.clone()));
        match peer {
            Some((meta, out)) => {
                meta.alone_since_ms.store(s.ms(now), Ordering::Relaxed);
                meta.set_class(CLASS_ALONE);
                let _ = out.send(&ServerMsg::PeerOffline);
            }
            None => ids.mark_idle(b.rendezvous_id, now),
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
            // Per-source buckets first, so one address (or prefix) can
            // neither drain the global budget nor reach it with junk.
            if !s.stun_limiter.allow(src.ip()) {
                continue;
            }
            let Some(req) = stun::parse_request(&buf[..n]) else {
                continue;
            };
            let found = {
                let mut creds = s.creds.lock().unwrap();
                let Some(c) = creds
                    .get_mut(req.username)
                    .filter(|c| c.expires > Instant::now())
                else {
                    continue;
                };
                // Only well-formed requests naming a live credential cost
                // global tokens, after that credential's own budget.
                if !c
                    .bucket
                    .take(s.cfg.stun_rate_per_cred, s.cfg.stun_burst_per_cred, 1.0)
                {
                    continue;
                }
                if !s.stun_global.lock().unwrap().take(
                    s.cfg.stun_rate_global,
                    s.cfg.stun_burst_global,
                    1.0,
                ) {
                    continue;
                }
                (c.password.clone(), c.bind)
            };
            let (password, bind) = found;
            if !req.verify(password.as_bytes()) {
                continue;
            }
            if let Some(b) = bind {
                let mut ids = s.ids.lock().unwrap();
                if let Some(e) = ids.map.get_mut(&b.rendezvous_id)
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

/// Drain a session's outbound queues to its socket, signaling first. Exits
/// (and kicks the session) when the queues close, a write fails or times
/// out, or the reader falls below the minimum drain rate.
async fn write_loop<W>(mut wr: W, mut q: WriterRx, pace: Pace, kick: CancellationToken)
where
    W: AsyncWrite + Unpin,
{
    let mut backlog = None;
    loop {
        let (frame, data, cost) = tokio::select! {
            biased;
            m = q.signal.recv() => match m {
                Some(b) => {
                    let c = item_cost(b.len());
                    (b, false, c)
                }
                None => break,
            },
            Some(packet) = q.data.recv() => {
                let c = item_cost(packet.len());
                (ServerMsg::Data { packet }.encode(), true, c)
            }
        };
        q.in_flight = Some((data, cost));
        let ok = write_paced(&mut wr, &frame, &mut backlog, &pace).await;
        if let Some((d, n)) = q.in_flight.take() {
            q.budget.release(d, n);
        }
        if !ok {
            break;
        }
        if q.budget.queued() == 0 {
            backlog = None;
        }
    }
    kick.cancel();
    drop(q);
    let _ = tokio::time::timeout(TEARDOWN_TIMEOUT, wr.shutdown()).await;
}

/// Write one frame within `write_timeout`. While the session has a backlog
/// (`backlog` = window start, bytes written in it), the socket must also
/// accept `min_rate` bytes/s per `window`, checked even mid-write.
async fn write_paced<W>(
    wr: &mut W,
    frame: &[u8],
    backlog: &mut Option<(Instant, usize)>,
    pace: &Pace,
) -> bool
where
    W: AsyncWrite + Unpin,
{
    let now = Instant::now();
    let (mut start, mut done) = *backlog.get_or_insert((now, 0));
    let hard = now + pace.write_timeout;
    let need = |d: Duration| pace.min_rate * d.as_secs_f64();
    let w = write_frame(wr, frame);
    tokio::pin!(w);
    loop {
        let window_end = start + pace.window;
        tokio::select! {
            r = &mut w => {
                if r.is_err() {
                    return false;
                }
                done += frame.len();
                break;
            }
            _ = tokio::time::sleep_until(window_end.min(hard).into()) => {
                if Instant::now() >= hard || (done as f64) < need(pace.window) {
                    return false;
                }
                start = window_end;
                done = 0;
            }
        }
    }
    let now = Instant::now();
    let elapsed = now.saturating_duration_since(start);
    if elapsed >= pace.window {
        if (done as f64) < need(elapsed) {
            return false;
        }
        start = now;
        done = 0;
    }
    *backlog = Some((start, done));
    true
}

/// Close the session's queues and give the writer a bounded flush.
async fn finish_writer(out: Outbox, mut writer: tokio::task::JoinHandle<()>) {
    drop(out);
    if tokio::time::timeout(TEARDOWN_TIMEOUT, &mut writer)
        .await
        .is_err()
    {
        writer.abort();
    }
}

/// Free the id's relay pair slot, if it holds one.
fn end_relay(s: &Shared, relaying: &mut HashSet<[u8; 32]>, rid: &[u8; 32], e: &mut IdEntry) {
    if let Some(h) = e.relay.take() {
        relaying.remove(rid);
        s.relay_pairs.fetch_sub(1, Ordering::SeqCst);
        for ip in h.ips {
            s.relay_ips.release(ip);
        }
        debug!("relay pair ended");
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
    let (sent_a, sent_b) = (a.out.send(&to_a), b.out.send(&to_b));
    if !(sent_a && sent_b) {
        warn!("punch notification dropped (slow peer)");
    }
    info!(round = e.punch_rounds, "punch coordinated");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::PairingSecret;
    use tokio::io::{AsyncReadExt, DuplexStream, ReadHalf, WriteHalf};

    const EX: [u8; EXPORTER_LEN] = [7; EXPORTER_LEN];

    fn cfg() -> RelayConfig {
        RelayConfig {
            auth_delay: Duration::from_millis(1),
            ..RelayConfig::default()
        }
    }

    enum Who<'a> {
        Decoy,
        As(&'a PairingSecret, Role),
    }

    struct Peer {
        rd: ReadHalf<DuplexStream>,
        wr: WriteHalf<DuplexStream>,
        task: tokio::task::JoinHandle<()>,
    }

    /// Admit a v2 session from `ip` over an in-memory pipe (`buf` bytes of
    /// socket buffer) and run the handshake. None if admission refused.
    async fn connect(relay: &Relay, ip: &str, who: Who<'_>, buf: usize) -> Option<Peer> {
        let meta = relay.admit(canonical_ip(ip.parse().unwrap()))?;
        let (client, server) = tokio::io::duplex(buf);
        let task = {
            let r = relay.clone();
            tokio::spawn(async move {
                let deadline = Instant::now() + r.shared.cfg.handshake_timeout;
                r.session(server, meta.clone(), 2, deadline, EX).await;
                r.finish(&meta);
            })
        };
        let (rd, wr) = tokio::io::split(client);
        let mut p = Peer { rd, wr, task };
        let keys = match who {
            Who::Decoy => {
                p.send_raw(&[0x01, 0xff]).await;
                None
            }
            Who::As(s, role) => {
                let k = s.derive();
                p.send(&ClientMsg::Hello {
                    role,
                    rendezvous_id: k.rendezvous_id,
                    public_key: k.public_key(),
                })
                .await;
                Some((k, role))
            }
        };
        let ServerMsg::Challenge { nonce } = p.wait_for(|_| true).await else {
            panic!("no challenge");
        };
        match keys {
            None => p.send_raw(&[0x99]).await,
            Some((k, role)) => {
                p.send(&ClientMsg::Auth {
                    signature: k.sign_challenge(&nonce, role, &EX),
                })
                .await
            }
        }
        p.wait_for(|m| matches!(m, ServerMsg::Registered { .. }))
            .await;
        Some(p)
    }

    impl Peer {
        async fn send_raw(&mut self, b: &[u8]) {
            write_frame(&mut self.wr, b).await.unwrap();
        }

        async fn send(&mut self, m: &ClientMsg) {
            self.send_raw(&m.encode()).await;
        }

        async fn wait_for(&mut self, want: impl Fn(&ServerMsg) -> bool) -> ServerMsg {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let f = read_frame(&mut self.rd).await.expect("relay closed");
                    let m = ServerMsg::decode(&f).unwrap();
                    if want(&m) {
                        return m;
                    }
                }
            })
            .await
            .expect("timed out")
        }

        /// Ping and wait for the Pong: everything sent before was handled.
        async fn sync(&mut self) {
            self.send(&ClientMsg::Ping).await;
            self.wait_for(|m| matches!(m, ServerMsg::Pong)).await;
        }

        /// True once the relay closed this session.
        async fn closed(&mut self) -> bool {
            tokio::time::timeout(Duration::from_secs(5), async {
                while read_frame(&mut self.rd).await.is_ok() {}
            })
            .await
            .is_ok()
        }

        async fn hang_up(self) {
            drop(self.rd);
            drop(self.wr);
            let _ = self.task.await;
        }
    }

    fn data(n: usize) -> ClientMsg {
        ClientMsg::Data {
            packet: vec![0xAB; n],
        }
    }

    #[tokio::test]
    async fn full_table_evicts_decoys_then_unpaired_never_paired() {
        let relay = Relay::new(
            RelayConfig {
                max_connections: 3,
                ..cfg()
            },
            3478,
        );
        let lone = PairingSecret::generate();
        let pair = PairingSecret::generate();
        let mut lonely = connect(&relay, "198.51.100.1", Who::As(&lone, Role::Box), 4096)
            .await
            .unwrap();
        let mut decoy = connect(&relay, "198.51.100.2", Who::Decoy, 4096)
            .await
            .unwrap();
        let mut bx = connect(&relay, "198.51.100.3", Who::As(&pair, Role::Box), 4096)
            .await
            .unwrap();
        // Full. The decoy goes first, though the lonely box is older.
        let mut dev = connect(&relay, "198.51.100.4", Who::As(&pair, Role::Device), 4096)
            .await
            .expect("admitted by evicting");
        assert!(decoy.closed().await, "decoy evicted");
        bx.wait_for(|m| matches!(m, ServerMsg::PeerOnline)).await;
        // Next: the oldest unpaired session, never the paired ones.
        let mut n5 = connect(&relay, "198.51.100.5", Who::Decoy, 4096)
            .await
            .unwrap();
        assert!(lonely.closed().await, "unpaired box evicted");
        let n6 = connect(&relay, "198.51.100.6", Who::Decoy, 4096)
            .await
            .unwrap();
        assert!(n5.closed().await);
        bx.sync().await;
        dev.sync().await;
        n6.hang_up().await;

        // Only paired sessions left: refuse rather than break a pairing.
        let relay = Relay::new(
            RelayConfig {
                max_connections: 2,
                ..cfg()
            },
            3478,
        );
        let mut a = connect(&relay, "198.51.100.1", Who::As(&pair, Role::Box), 4096)
            .await
            .unwrap();
        let _b = connect(&relay, "198.51.100.2", Who::As(&pair, Role::Device), 4096)
            .await
            .unwrap();
        a.wait_for(|m| matches!(m, ServerMsg::PeerOnline)).await;
        assert!(relay.admit("198.51.100.3".parse().unwrap()).is_none());
    }

    #[tokio::test]
    async fn per_address_cap_evicts_within_the_address() {
        let relay = Relay::new(
            RelayConfig {
                max_connections_per_ip: 2,
                ..cfg()
            },
            3478,
        );
        let mut d1 = connect(&relay, "203.0.113.7", Who::Decoy, 4096)
            .await
            .unwrap();
        let mut other = connect(&relay, "198.51.100.1", Who::Decoy, 4096)
            .await
            .unwrap();
        let _d2 = connect(&relay, "203.0.113.7", Who::Decoy, 4096)
            .await
            .unwrap();
        // A third session behind the same (carrier-NAT) address displaces
        // that address's oldest decoy, not another address's.
        let _d3 = connect(&relay, "::ffff:203.0.113.7", Who::Decoy, 4096)
            .await
            .unwrap();
        assert!(d1.closed().await);
        other.sync().await;
    }

    #[tokio::test]
    async fn slow_reader_is_byte_capped_then_cut_off() {
        let relay = Relay::new(
            RelayConfig {
                queue_data_bytes: 8 * 1024,
                min_drain_rate: 16.0 * 1024.0,
                drain_window: Duration::from_millis(400),
                ..cfg()
            },
            3478,
        );
        let s = PairingSecret::generate();
        let mut bx = connect(&relay, "198.51.100.1", Who::As(&s, Role::Box), 256 * 1024)
            .await
            .unwrap();
        let dev = connect(&relay, "198.51.100.2", Who::As(&s, Role::Device), 256)
            .await
            .unwrap();
        bx.wait_for(|m| matches!(m, ServerMsg::PeerOnline)).await;
        // The device reads ~2 KB/s: every write completes well inside the
        // write timeout, but far below the drain rate.
        let Peer {
            rd: mut dev_rd,
            wr: dev_wr,
            task: dev_task,
        } = dev;
        let slow = tokio::spawn(async move {
            let mut buf = [0u8; 100];
            loop {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if matches!(dev_rd.read(&mut buf).await, Ok(0) | Err(_)) {
                    break;
                }
            }
        });
        for _ in 0..64 {
            bx.send(&data(1024)).await;
        }
        bx.wait_for(|m| matches!(m, ServerMsg::PeerOffline)).await;
        let st = relay.relay_stats();
        assert!(st.dropped_queue > 0, "byte cap never hit: {st:?}");
        assert!(st.packets <= 8, "queued past the byte cap: {st:?}");
        tokio::time::timeout(Duration::from_secs(5), dev_task)
            .await
            .expect("device session ended")
            .unwrap();
        slow.abort();
        drop(dev_wr);
        bx.hang_up().await;
        assert_eq!(
            relay.shared.queued.load(Ordering::SeqCst),
            0,
            "global bytes leaked"
        );
    }

    #[tokio::test]
    async fn new_ids_are_budgeted_per_address_and_never_paired_expire_fast() {
        let relay = Relay::new(
            RelayConfig {
                new_id_rate_per_ip: 0.0,
                new_id_burst_per_ip: 2.0,
                ..cfg()
            },
            3478,
        );
        let a = "198.51.100.1";
        let secrets: Vec<_> = (0..3).map(|_| PairingSecret::generate()).collect();
        for s in &secrets {
            connect(&relay, a, Who::As(s, Role::Box), 4096)
                .await
                .unwrap()
                .hang_up()
                .await;
        }
        assert_eq!(
            relay.id_count(),
            2,
            "third new id from one address is a decoy"
        );
        // Re-registering a known id costs nothing; other addresses have
        // their own budget.
        connect(&relay, a, Who::As(&secrets[0], Role::Box), 4096)
            .await
            .unwrap()
            .hang_up()
            .await;
        let pair = PairingSecret::generate();
        let mut bx = connect(&relay, "198.51.100.2", Who::As(&pair, Role::Box), 4096)
            .await
            .unwrap();
        let dev = connect(&relay, "198.51.100.3", Who::As(&pair, Role::Device), 4096)
            .await
            .unwrap();
        bx.wait_for(|m| matches!(m, ServerMsg::PeerOnline)).await;
        assert_eq!(relay.id_count(), 3);
        bx.hang_up().await;
        dev.hang_up().await;

        let now = Instant::now();
        relay.housekeep(now + Duration::from_secs(11 * 60));
        assert_eq!(relay.id_count(), 1, "never-paired ids expire after 10 min");
        relay.housekeep(now + Duration::from_secs(23 * 3600));
        assert_eq!(relay.id_count(), 1, "paired ids keep 24 h");
        relay.housekeep(now + Duration::from_secs(25 * 3600));
        assert_eq!(relay.id_count(), 0);

        // A full table drops its longest-idle id for a newcomer instead of
        // turning the newcomer into a decoy.
        let relay = Relay::new(
            RelayConfig {
                max_ids: 1,
                ..cfg()
            },
            3478,
        );
        connect(&relay, a, Who::As(&secrets[0], Role::Box), 4096)
            .await
            .unwrap()
            .hang_up()
            .await;
        let mut bx = connect(&relay, a, Who::As(&pair, Role::Box), 4096)
            .await
            .unwrap();
        let _dev = connect(&relay, "198.51.100.3", Who::As(&pair, Role::Device), 4096)
            .await
            .unwrap();
        bx.wait_for(|m| matches!(m, ServerMsg::PeerOnline)).await;
        assert_eq!(relay.id_count(), 1);
    }

    #[tokio::test]
    async fn relay_slots_go_to_paired_sessions_fairly() {
        let relay = Relay::new(
            RelayConfig {
                relay_max_pairs: 2,
                ..cfg()
            },
            3478,
        );
        // Datagrams from a decoy or a peer whose partner is offline never
        // take a slot or bandwidth.
        let mut decoy = connect(&relay, "198.51.100.9", Who::Decoy, 4096)
            .await
            .unwrap();
        let lone = PairingSecret::generate();
        let mut l = connect(&relay, "198.51.100.10", Who::As(&lone, Role::Box), 4096)
            .await
            .unwrap();
        for p in [&mut decoy, &mut l] {
            p.send(&data(64)).await;
            p.sync().await;
        }
        let st = relay.relay_stats();
        assert_eq!((st.active_pairs, st.dropped_no_peer), (0, 2));

        async fn relaying_pair(relay: &Relay, ips: [&str; 2]) -> (Peer, Peer) {
            let s = PairingSecret::generate();
            let mut b = connect(relay, ips[0], Who::As(&s, Role::Box), 64 * 1024)
                .await
                .unwrap();
            let d = connect(relay, ips[1], Who::As(&s, Role::Device), 64 * 1024)
                .await
                .unwrap();
            b.wait_for(|m| matches!(m, ServerMsg::PeerOnline)).await;
            b.send(&data(64)).await;
            b.sync().await;
            (b, d)
        }
        // Two pairs entirely behind one address fill the table...
        let hog = "203.0.113.1";
        let (_h1, mut h1d) = relaying_pair(&relay, [hog, hog]).await;
        let (_h2, _h2d) = relaying_pair(&relay, [hog, hog]).await;
        h1d.wait_for(|m| matches!(m, ServerMsg::Data { .. })).await;
        assert_eq!(relay.relay_stats().active_pairs, 2);
        // ...yet a pair from two other addresses still gets a slot: the
        // busiest address gives one up.
        let (_b, mut d) = relaying_pair(&relay, ["192.0.2.1", "192.0.2.2"]).await;
        d.wait_for(|m| matches!(m, ServerMsg::Data { .. })).await;
        let st = relay.relay_stats();
        assert_eq!(st.active_pairs, 2);
        assert_eq!(st.dropped_limit, 0);

        // Per-address cap: a pair with both ends behind one address counts
        // twice, so a cap of 2 fits one such pair.
        let relay = Relay::new(
            RelayConfig {
                relay_max_pairs_per_ip: 2,
                ..cfg()
            },
            3478,
        );
        let (_p1, _p1d) = relaying_pair(&relay, [hog, hog]).await;
        let (_p2, _p2d) = relaying_pair(&relay, [hog, hog]).await;
        let st = relay.relay_stats();
        assert_eq!((st.active_pairs, st.dropped_limit), (1, 1));
    }
}
