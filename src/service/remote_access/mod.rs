//! Remote access via the relay (relay.peckboard.com).
//!
//! For every paired device ([`crate::db::models::RemoteDevice`]) the box
//! keeps a box-role rendezvous registration alive; when the device shows
//! up, the relay library punches a direct UDP path and the tunnel is
//! served into Peckboard's own router (see [`tunnel`] for why that is not
//! a plain forward to the loopback HTTP port). Failures retry with
//! exponential backoff; per-device status is held in memory for the UI.
//!
//! Direct connection: behind a symmetric NAT no punch gets through, so
//! the admin can configure a UDP port range to forward on the router.
//! Each device loop leases the lowest free port of the range for its
//! lifetime ([`PortPool`]), binds its punch socket there, and advertises
//! it on the public address (configured, or the STUN-observed IP).
//!
//! Globally off by default ([`RemoteAccessSettings::enabled`]); the
//! setting, the relay host, and every device are admin-only
//! (`routes::remote_access`).

pub mod relay;
pub mod secret;
pub mod tunnel;

use std::collections::{BTreeSet, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use axum::Router;
use peckboard_relay::keys::RendezvousSecret;
use peckboard_relay::tunnel::{EnrollMode, LinkMode, RefuseReason, async_trait};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use crate::db::Db;
use crate::db::crud::{
    ActivateAttempt, EnrollAttempt, EnrollOutcome, EnrollRefusal, rfc3339_after,
};
use crate::db::models::{RemoteDeviceEnrollment, enrollment_state};
use crate::routes::settings::{SETTINGS_COLLECTION, SETTINGS_NS};
use secret::DeviceSecret;
use tunnel::{
    BoxCredential, BoxIdentity, DirectOptions, EnrollHandler, IdentityStatus, Registered,
    TunnelBackend, TunnelUpdate,
};

const SETTINGS_KEY: &str = "remote_access";
pub const DEFAULT_RELAY_HOST: &str = "relay.peckboard.com";
pub const DEFAULT_UDP_PORT_COUNT: u16 = 10;
pub const MAX_UDP_PORT_COUNT: u16 = 256;
const BACKOFF_MIN: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// The box's permanent identity key for relay registration
/// (`<data_dir>/remote_access_identity`), created on first enable.
pub const IDENTITY_FILE: &str = "remote_access_identity";
/// Least gap between two registration-status requests to the relay (its
/// limit is 0.5 req/s per IP, burst 20).
const REGISTRATION_POLL_MIN: Duration = Duration::from_secs(5);
/// Background poll cadence while waiting for the admin to register.
const REGISTRATION_POLL_EVERY: Duration = Duration::from_secs(10);
/// How long after an enable or an explicit refresh the box keeps polling.
const REGISTRATION_WATCH: Duration = Duration::from_secs(180);
/// How long a v2 pairing link may be used (`PECKBOARD_DEV_LINK_TTL_SECS`
/// overrides it; dev / e2e only).
pub const LINK_TTL: Duration = Duration::from_secs(60 * 60);
/// After a link is used (or expired) the `rid(S)` loop keeps answering
/// "already used" / "expired" for this long, then stops.
pub const LINK_REFUSE_GRACE: Duration = Duration::from_secs(24 * 60 * 60);
/// Loops that wait for a device block indefinitely, so expiry passes are
/// driven by this tick as well as by DB changes.
const RECONCILE_TICK: Duration = Duration::from_secs(60);
/// Status of every enrolled device while the box identity file is gone:
/// a new key would lock them out, so none is generated.
pub const IDENTITY_MISSING: &str = "Box identity key missing — re-pair this device";
/// Hidden dev knob: link TTL in seconds (default [`LINK_TTL`]). When set,
/// `POST /api/remote-access/devices` also honours a per-link `ttl_secs`.
pub const DEV_LINK_TTL_ENV: &str = "PECKBOARD_DEV_LINK_TTL_SECS";

/// The link TTL in force ([`LINK_TTL`] unless [`DEV_LINK_TTL_ENV`] is set).
pub fn link_ttl() -> Duration {
    std::env::var(DEV_LINK_TTL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map_or(LINK_TTL, Duration::from_secs)
}

/// Whether the dev TTL knob is on (so per-link overrides are accepted).
pub fn dev_link_ttl_enabled() -> bool {
    std::env::var_os(DEV_LINK_TTL_ENV).is_some()
}

/// What the UI shows for a device's pairing: `legacy` (no enrollment row,
/// S-only), `pending` (unused v2 link), `expired`, `staged` (enrolled, not
/// yet seen on `R`) or `enrolled`.
pub fn enrollment_view_state(enr: Option<&RemoteDeviceEnrollment>, now: &str) -> &'static str {
    match enr {
        None => "legacy",
        Some(e) if e.state == enrollment_state::PENDING => {
            if e.link_expires_at
                .as_deref()
                .is_none_or(|exp| rfc3339_after(now, exp))
            {
                "expired"
            } else {
                "pending"
            }
        }
        Some(e) if e.state == enrollment_state::STAGED => "staged",
        Some(_) => "enrolled",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteAccessSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_relay_host")]
    pub relay_host: String,
    /// First UDP port of the forwarded range; `None`: ephemeral ports.
    #[serde(default)]
    pub udp_port_base: Option<u16>,
    #[serde(default = "default_udp_port_count")]
    pub udp_port_count: u16,
    /// Host/IP the forwarded ports are reachable on; empty: the
    /// STUN-observed public IP.
    #[serde(default)]
    pub public_address: String,
}

fn default_relay_host() -> String {
    DEFAULT_RELAY_HOST.to_string()
}

fn default_udp_port_count() -> u16 {
    DEFAULT_UDP_PORT_COUNT
}

impl Default for RemoteAccessSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            relay_host: default_relay_host(),
            udp_port_base: None,
            udp_port_count: DEFAULT_UDP_PORT_COUNT,
            public_address: String::new(),
        }
    }
}

/// `host` or `host:port`, DNS-name / IP-literal characters only.
pub fn validate_relay_host(h: &str) -> Result<(), &'static str> {
    let (host, port) = match h.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (h, None),
    };
    if host.is_empty()
        || h.len() > 253
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return Err("relay host must be a hostname, optionally with :port");
    }
    if let Some(p) = port
        && p.parse::<u16>().map_or(true, |p| p == 0)
    {
        return Err("relay port must be 1..=65535");
    }
    Ok(())
}

/// The validated direct-connection settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectSettings {
    pub udp_port_base: Option<u16>,
    pub udp_port_count: u16,
    pub public_address: String,
}

/// Check the direct-connection fields; `Err((field, message))` names the
/// offending field. `public_address` is a hostname or IP literal (no port:
/// the leased port is advertised on it) and needs a port range.
pub fn validate_direct(
    udp_port_base: Option<i64>,
    udp_port_count: i64,
    public_address: &str,
) -> Result<DirectSettings, (&'static str, &'static str)> {
    let base = match udp_port_base {
        None => None,
        Some(b) if (1024..=65535).contains(&b) => Some(b as u16),
        Some(_) => return Err(("udp_port_base", "UDP port must be 1024–65535")),
    };
    if !(1..=MAX_UDP_PORT_COUNT as i64).contains(&udp_port_count) {
        return Err(("udp_port_count", "Range size must be 1–256"));
    }
    if let Some(b) = base
        && b as i64 + udp_port_count - 1 > 65535
    {
        return Err(("udp_port_count", "Range runs past port 65535"));
    }
    let public = public_address.trim();
    if !public.is_empty() {
        let hostname = public.len() <= 253
            && !public.starts_with(['.', '-'])
            && !public.ends_with(['.', '-'])
            && public
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
        if public.parse::<IpAddr>().is_err() && !hostname {
            return Err((
                "public_address",
                "Public address must be a hostname or IP address, without a port",
            ));
        }
        if base.is_none() {
            return Err((
                "public_address",
                "Set a UDP port first — the public address advertises it",
            ));
        }
    }
    Ok(DirectSettings {
        udp_port_base: base,
        udp_port_count: udp_port_count as u16,
        public_address: public.to_string(),
    })
}

/// UDP ports of the configured range held by running device loops.
#[derive(Clone, Default)]
pub struct PortPool(Arc<Mutex<BTreeSet<u16>>>);

/// One port of a [`PortPool`], released when dropped (the device loop
/// holding it stopped, was revoked, or was restarted).
pub struct PortLease {
    pool: PortPool,
    port: u16,
}

impl PortPool {
    /// The lowest free port in `base..base + count`, or `None` when every
    /// port of the range is leased.
    pub fn lease(&self, base: u16, count: u16) -> Option<PortLease> {
        let mut used = self.0.lock().unwrap();
        let port = (base as u32..base as u32 + count as u32)
            .filter(|p| *p <= u16::MAX as u32)
            .map(|p| p as u16)
            .find(|p| !used.contains(p))?;
        used.insert(port);
        Some(PortLease {
            pool: self.clone(),
            port,
        })
    }
}

impl PortLease {
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for PortLease {
    fn drop(&mut self) {
        self.pool.0.lock().unwrap().remove(&self.port);
    }
}

/// Live per-device tunnel state (memory only).
#[derive(Debug, Clone, Serialize)]
pub struct DeviceStatus {
    /// `offline` (remote access disabled) | `waiting` (registered with the
    /// relay, device not connected) | `connected` | `error` (retrying).
    pub state: &'static str,
    pub peer: Option<String>,
    pub rtt_ms: Option<u32>,
    pub error: Option<String>,
    /// Local UDP port of the current registration.
    pub local_port: Option<u16>,
    /// Candidates advertised to the device (LAN + forwarded address).
    pub candidates: Vec<String>,
    /// While connected: `direct` (hole-punched) | `relayed` (through the
    /// relay — end-to-end encrypted; punching failed).
    pub path: Option<&'static str>,
}

impl DeviceStatus {
    fn offline() -> Self {
        Self {
            state: "offline",
            peer: None,
            rtt_ms: None,
            error: None,
            local_port: None,
            path: None,
            candidates: Vec::new(),
        }
    }
    fn waiting() -> Self {
        Self {
            state: "waiting",
            ..Self::offline()
        }
    }
    fn error(msg: String) -> Self {
        Self {
            state: "error",
            error: Some(msg),
            ..Self::offline()
        }
    }
}

/// Relay registration of the box identity, as the API reports it
/// (`GET /api/remote-access` → `registration`). `supported` stays false
/// until the relay has said anything — one that predates relay
/// registration never does, and the UI then shows nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RegistrationView {
    pub supported: bool,
    pub registered: Option<bool>,
    pub gated: Option<bool>,
    /// The relay's registration page for this box (empty: no identity yet).
    pub url: String,
}

/// What the box knows about its relay registration. The latest word wins:
/// a box handshake reports `registered` + `gated`, a status poll
/// `registered` only.
#[derive(Default)]
struct RegistrationState {
    /// The relay's verdict at the last box handshake — fixed per session,
    /// so stale once the admin registers until the loops re-establish.
    handshake: Option<IdentityStatus>,
    registered: Option<bool>,
    gated: Option<bool>,
    last_poll: Option<Instant>,
    /// Poll in the background until then (`None`: not waiting).
    watch_until: Option<Instant>,
    /// A background poll task is running.
    watching: bool,
}

impl RegistrationState {
    fn note_handshake(&mut self, s: IdentityStatus) {
        self.handshake = Some(s);
        self.registered = Some(s.registered);
        self.gated = Some(s.gated);
    }

    fn view(&self, url: String) -> RegistrationView {
        RegistrationView {
            supported: self.registered.is_some(),
            registered: self.registered,
            gated: self.gated,
            url,
        }
    }
}

/// Which of a device's two loops a task is: the `rid(S)` loop (legacy
/// service, link enrollment, or refusal) or the `rid(R)` loop of an
/// enrolled device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    S,
    R,
}

impl Slot {
    /// `inner.tasks` key: `<device_id>/s` or `<device_id>/r`.
    fn key(self, device_id: &str) -> String {
        match self {
            Slot::S => format!("{device_id}/s"),
            Slot::R => format!("{device_id}/r"),
        }
    }
}

/// What a loop serves, decided from the DB rows alone (no secrets opened)
/// so `reconcile` can compare it with what is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Plan {
    /// S-only pairing: full service on `/1`, upgrade offered.
    Legacy,
    /// Unused or staged v2 link: enrollment only.
    LinkEnroll,
    /// Used or expired link: every request refused with this reason.
    LinkRefuse(RefuseReason),
    /// `rid(R)`, box cert `B`, client cert `D`.
    Enrolled,
}

impl Plan {
    /// Legacy loops run without an identity (no upgrade offered then);
    /// everything v2 needs `B`.
    fn needs_identity(self) -> bool {
        !matches!(self, Plan::Legacy)
    }

    /// Only loops that serve traffic lease a forwarded port.
    fn leases_port(self) -> bool {
        matches!(self, Plan::Legacy | Plan::Enrolled)
    }
}

fn rfc3339_plus(stamp: &str, d: Duration) -> Option<String> {
    let t = chrono::DateTime::parse_from_rfc3339(stamp).ok()?;
    Some((t + chrono::Duration::from_std(d).ok()?).to_rfc3339())
}

/// The loop `slot` should run for a device in this enrollment state at
/// `now` (`None`: no loop). See the state table in the pairing-v2 design.
fn plan(slot: Slot, enr: Option<&RemoteDeviceEnrollment>, now: &str) -> Option<Plan> {
    match (slot, enr) {
        (Slot::S, None) => Some(Plan::Legacy),
        (Slot::S, Some(e)) if e.state == enrollment_state::PENDING => {
            let exp = e.link_expires_at.as_deref()?;
            if !rfc3339_after(now, exp) {
                Some(Plan::LinkEnroll)
            } else if rfc3339_plus(exp, LINK_REFUSE_GRACE)
                .is_some_and(|until| !rfc3339_after(now, &until))
            {
                Some(Plan::LinkRefuse(RefuseReason::Expired))
            } else {
                None
            }
        }
        (Slot::S, Some(e)) if e.state == enrollment_state::STAGED => {
            // A link re-delivers R to the enrolled key (crash recovery); a
            // legacy upgrade keeps serving the device on S until it moves.
            if e.link_expires_at.is_some() {
                Some(Plan::LinkEnroll)
            } else {
                Some(Plan::Legacy)
            }
        }
        (Slot::S, Some(e)) => {
            let refusing = e.link_secret_ciphertext.is_some()
                && e.link_refuse_until
                    .as_deref()
                    .is_some_and(|until| !rfc3339_after(now, until));
            refusing.then_some(Plan::LinkRefuse(RefuseReason::AlreadyUsed))
        }
        (Slot::R, Some(e))
            if e.state == enrollment_state::STAGED || e.state == enrollment_state::ACTIVE =>
        {
            Some(Plan::Enrolled)
        }
        (Slot::R, _) => None,
    }
}

struct LoopTask {
    plan: Plan,
    handle: JoinHandle<()>,
}

/// Keyed by loop (`Slot::key`), not by device: an enrolled device runs a
/// `rid(R)` loop and, for a while, a refusing `rid(S)` loop.
#[derive(Default)]
struct Inner {
    tasks: HashMap<String, LoopTask>,
    status: HashMap<String, DeviceStatus>,
    /// The last registration per loop, merged into its status.
    registered: HashMap<String, Registered>,
    registration: RegistrationState,
}

pub struct RemoteAccess {
    db: Db,
    vault_key: Vec<u8>,
    backend: Arc<dyn TunnelBackend>,
    /// Peckboard's router, bound once the server has built it.
    app: OnceLock<Router>,
    inner: Mutex<Inner>,
    ports: PortPool,
    /// `<data_dir>/remote_access_identity`; loaded or created on demand.
    identity_path: PathBuf,
    identity: Mutex<Option<BoxIdentity>>,
    ticking: AtomicBool,
}

impl RemoteAccess {
    pub fn new(
        db: Db,
        vault_key: Vec<u8>,
        data_dir: &Path,
        backend: Arc<dyn TunnelBackend>,
    ) -> Arc<Self> {
        Arc::new(Self {
            db,
            vault_key,
            backend,
            app: OnceLock::new(),
            inner: Mutex::new(Inner::default()),
            ports: PortPool::default(),
            identity_path: data_dir.join(IDENTITY_FILE),
            identity: Mutex::new(None),
            ticking: AtomicBool::new(false),
        })
    }

    /// A manager over its own empty in-memory DB that is never started, so
    /// it never contacts a relay (nor writes an identity) — for test
    /// harnesses building `AppState`.
    pub fn inert() -> Arc<Self> {
        Self::new(
            Db::in_memory().expect("in-memory db"),
            vec![0u8; 32],
            &std::env::temp_dir().join("peckboard-inert"),
            Arc::new(relay::RelayBackend),
        )
    }
    pub fn vault_key(&self) -> &[u8] {
        &self.vault_key
    }

    /// Bind the router tunnels are served into and start every device
    /// loop the stored setting calls for; from then on a periodic tick
    /// retires expired links and refuse loops.
    pub async fn start(self: &Arc<Self>, app: Router) {
        let _ = self.app.set(app);
        self.reconcile().await;
        if !self.ticking.swap(true, Ordering::SeqCst) {
            let weak = Arc::downgrade(self);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(RECONCILE_TICK).await;
                    let Some(this) = weak.upgrade() else { return };
                    let now = chrono::Utc::now().to_rfc3339();
                    match this.db.clear_expired_remote_link_secrets(&now).await {
                        Ok(n) if n > 0 => {
                            tracing::info!("remote access: {n} refuse loop(s) retired")
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!("remote access: retiring refuse loops: {e}"),
                    }
                    this.reconcile().await;
                }
            });
        }
    }

    pub async fn settings(&self) -> RemoteAccessSettings {
        let db = self.db.clone();
        let raw = tokio::task::spawn_blocking(move || {
            db.plugin_store_get_blocking(SETTINGS_NS, SETTINGS_COLLECTION, SETTINGS_KEY)
        })
        .await;
        match raw {
            Ok(Ok(Some(json))) => serde_json::from_str(&json).unwrap_or_default(),
            _ => RemoteAccessSettings::default(),
        }
    }

    /// Persist new settings and restart every device loop under them.
    pub async fn put_settings(self: &Arc<Self>, s: &RemoteAccessSettings) -> anyhow::Result<()> {
        let before = self.settings().await;
        let db = self.db.clone();
        let value = serde_json::to_string(s)?;
        tokio::task::spawn_blocking(move || {
            db.plugin_store_put_blocking(SETTINGS_NS, SETTINGS_COLLECTION, SETTINGS_KEY, &value)
        })
        .await??;
        self.stop_all();
        if before.relay_host != s.relay_host {
            // Registration is per relay; a running watch picks the new
            // deadline up from `reconcile`.
            let mut inner = self.inner.lock().unwrap();
            let watching = inner.registration.watching;
            inner.registration = RegistrationState {
                watching,
                ..RegistrationState::default()
            };
        }
        self.reconcile().await;
        Ok(())
    }

    /// A device's live status: its `rid(R)` loop's once enrolled, else its
    /// `rid(S)` loop's.
    pub fn status(&self, device_id: &str) -> DeviceStatus {
        let inner = self.inner.lock().unwrap();
        let pick = |slot: Slot| {
            let key = slot.key(device_id);
            inner.status.get(&key).cloned().map(|s| (key, s))
        };
        let Some((key, mut s)) = pick(Slot::R).or_else(|| pick(Slot::S)) else {
            return DeviceStatus::offline();
        };
        if let Some(r) = inner.registered.get(&key) {
            s.local_port = Some(r.local_port);
            s.candidates = r.candidates.iter().map(|a| a.to_string()).collect();
        }
        s
    }

    fn set_status(&self, key: &str, s: DeviceStatus) {
        let mut inner = self.inner.lock().unwrap();
        // A revoked/stopped loop may race one last update in.
        if inner.tasks.contains_key(key) {
            inner.status.insert(key.to_string(), s);
        }
    }

    /// A connected tunnel moved between the direct and relayed path.
    fn set_path(&self, key: &str, path: &'static str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(s) = inner.status.get_mut(key)
            && s.state == "connected"
        {
            s.path = Some(path);
        }
    }

    fn set_registered(&self, key: &str, r: Registered) {
        let mut inner = self.inner.lock().unwrap();
        if inner.tasks.contains_key(key) {
            if let Some(s) = r.identity {
                inner.registration.note_handshake(s);
            }
            inner.registered.insert(key.to_string(), r);
        }
    }

    /// Stop one device's loops and drop their tunnels (every open
    /// connection through them dies with the task, and the UDP port lease
    /// with it).
    pub fn stop_device(&self, device_id: &str) {
        let mut inner = self.inner.lock().unwrap();
        for slot in [Slot::S, Slot::R] {
            let key = slot.key(device_id);
            if let Some(t) = inner.tasks.remove(&key) {
                t.handle.abort();
            }
            inner.status.remove(&key);
            inner.registered.remove(&key);
        }
    }

    fn stop_all(&self) {
        let mut inner = self.inner.lock().unwrap();
        for (_, t) in inner.tasks.drain() {
            t.handle.abort();
        }
        inner.status.clear();
        inner.registered.clear();
        // The sessions that verdict described are gone.
        inner.registration.handshake = None;
    }

    /// Bring running loops in line with the setting and the DB: none when
    /// disabled; otherwise, per device, the loops its enrollment state
    /// calls for ([`plan`]), restarting any whose plan changed.
    pub async fn reconcile(self: &Arc<Self>) {
        let Some(app) = self.app.get().cloned() else {
            return;
        };
        let settings = self.settings().await;
        if !settings.enabled {
            self.stop_all();
            self.inner.lock().unwrap().registration.watch_until = None;
            return;
        }
        let devices = match self.db.list_remote_devices().await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("remote access: listing devices failed: {e}");
                return;
            }
        };
        let enrollments = match self.db.list_remote_device_enrollments().await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("remote access: listing enrollments failed: {e}");
                return;
            }
        };
        // Created on first enable: the relay may require a registered box
        // for the relayed fallback, and v2 links pin it. Once any device
        // pins it, a missing file is never replaced (it would lock every
        // enrolled device out); those loops report IDENTITY_MISSING.
        let identity = match self.identity(enrollments.is_empty()) {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!("remote access: {e:#}; relay registration unavailable");
                None
            }
        };
        let enr_by_id: HashMap<&str, &RemoteDeviceEnrollment> = enrollments
            .iter()
            .map(|e| (e.device_id.as_str(), e))
            .collect();
        let now = chrono::Utc::now().to_rfc3339();
        let mut wanted: Vec<(String, String, Slot, Plan)> = Vec::new();
        for d in &devices {
            let enr = enr_by_id.get(d.id.as_str()).copied();
            for slot in [Slot::S, Slot::R] {
                if let Some(p) = plan(slot, enr, &now) {
                    wanted.push((slot.key(&d.id), d.id.clone(), slot, p));
                }
            }
        }
        let wanted_plan: HashMap<&str, Plan> =
            wanted.iter().map(|(k, _, _, p)| (k.as_str(), *p)).collect();
        let mut inner = self.inner.lock().unwrap();
        let stale: Vec<String> = inner
            .tasks
            .iter()
            .filter(|(k, t)| wanted_plan.get(k.as_str()) != Some(&t.plan) || t.handle.is_finished())
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            if let Some(t) = inner.tasks.remove(&k) {
                t.handle.abort();
            }
            inner.status.remove(&k);
            inner.registered.remove(&k);
        }
        // Placeholders (IDENTITY_MISSING) of loops no longer wanted.
        inner
            .status
            .retain(|k, _| wanted_plan.contains_key(k.as_str()));
        for (key, id, slot, p) in wanted {
            if inner.tasks.contains_key(&key) {
                continue;
            }
            if p.needs_identity() && identity.is_none() {
                tracing::warn!(device_id = %id, "remote access: {IDENTITY_MISSING}");
                inner
                    .status
                    .insert(key, DeviceStatus::error(IDENTITY_MISSING.into()));
                continue;
            }
            let this = self.clone();
            let settings = settings.clone();
            let app = app.clone();
            let identity = identity.clone();
            inner.status.insert(key.clone(), DeviceStatus::waiting());
            let handle = tokio::spawn(async move {
                this.device_loop(id, slot, p, settings, app, identity).await
            });
            inner.tasks.insert(key, LoopTask { plan: p, handle });
        }
        drop(inner);
        self.watch_registration();
    }

    /// Re-run [`reconcile`](Self::reconcile) from a task of its own — for
    /// callers inside a device loop, which `reconcile` may abort.
    fn reconcile_soon(self: &Arc<Self>) {
        let this = self.clone();
        tokio::spawn(async move { this.reconcile().await });
    }

    /// Stop a device's loops and start the ones its rows call for now
    /// (after a link re-issue: the running `rid(S)` loop holds the old S).
    pub async fn restart_device(self: &Arc<Self>, device_id: &str) {
        self.stop_device(device_id);
        self.reconcile().await;
    }

    /// The box's identity key: cached, else loaded from `identity_path`,
    /// else — when `create` — generated there. `Ok(None)`: no file yet.
    fn identity(&self, create: bool) -> anyhow::Result<Option<BoxIdentity>> {
        let mut slot = self.identity.lock().unwrap();
        if let Some(id) = &*slot {
            return Ok(Some(id.clone()));
        }
        match BoxIdentity::load_or_create_with(&self.identity_path, create) {
            Ok(id) => {
                *slot = Some(id.clone());
                Ok(Some(id))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !create => Ok(None),
            Err(e) => {
                Err(e).with_context(|| format!("box identity {}", self.identity_path.display()))
            }
        }
    }

    /// Whether the identity file may be created now: only while no device
    /// pins it (no enrollment row at all).
    async fn may_create_identity(&self) -> bool {
        matches!(self.db.list_remote_device_enrollments().await, Ok(e) if e.is_empty())
    }

    /// The identity a new v2 link pins: created on first use, never
    /// regenerated once a device pins it.
    pub async fn link_identity(&self) -> anyhow::Result<BoxIdentity> {
        let create = self.may_create_identity().await;
        self.identity(create)?.ok_or_else(|| {
            anyhow::anyhow!(
                "{IDENTITY_MISSING}: restore {} or revoke every enrolled device first",
                self.identity_path.display()
            )
        })
    }

    /// Fingerprint of the box identity, `None` until it exists.
    pub fn box_fingerprint(&self) -> Option<String> {
        self.identity(false)
            .ok()
            .flatten()
            .map(|id| id.fingerprint())
    }

    /// What the box knows about its relay registration; `relay_host` builds
    /// the registration page URL. Never creates the identity.
    pub fn registration(&self, relay_host: &str) -> RegistrationView {
        let Ok(Some(id)) = self.identity(false) else {
            return RegistrationView {
                supported: false,
                registered: None,
                gated: None,
                url: String::new(),
            };
        };
        let url = peckboard_relay::tunnel::registration_url(relay_host, &id.public_key());
        self.inner.lock().unwrap().registration.view(url)
    }

    /// `POST /api/remote-access/registration/refresh`: create the identity
    /// if needed, ask the relay now (rate-limited), keep polling in the
    /// background for a while, and report what is known.
    pub async fn refresh_registration(self: &Arc<Self>) -> RegistrationView {
        let relay_host = self.settings().await.relay_host;
        let create = self.may_create_identity().await;
        if let Err(e) = self.identity(create) {
            tracing::warn!("remote access: {e:#}; relay registration unavailable");
        }
        self.watch_registration();
        self.poll_registration(&relay_host).await;
        self.registration(&relay_host)
    }

    /// One registration-status request to the relay, unless one went out
    /// less than [`REGISTRATION_POLL_MIN`] ago. When the relay says the box
    /// got registered since the loops last handshook, they re-establish so
    /// their sessions carry the new verdict (it is fixed per session).
    async fn poll_registration(self: &Arc<Self>, relay_host: &str) {
        let Ok(Some(id)) = self.identity(false) else {
            return;
        };
        {
            let mut inner = self.inner.lock().unwrap();
            let reg = &mut inner.registration;
            if reg
                .last_poll
                .is_some_and(|t| t.elapsed() < REGISTRATION_POLL_MIN)
            {
                return;
            }
            reg.last_poll = Some(Instant::now());
        }
        match self
            .backend
            .registration_status(relay_host, &id.public_key())
            .await
        {
            Ok(registered) => {
                let restart = {
                    let mut inner = self.inner.lock().unwrap();
                    let reg = &mut inner.registration;
                    reg.registered = Some(registered);
                    registered && reg.handshake.is_some_and(|h| !h.registered)
                };
                if restart {
                    tracing::info!("remote access: box registered with the relay; re-establishing");
                    self.stop_all();
                    self.reconcile().await;
                }
            }
            Err(e) => {
                tracing::debug!("remote access: registration status unavailable: {e:#}");
            }
        }
    }

    /// Poll the relay in the background for [`REGISTRATION_WATCH`]
    /// (extending a running watch), until it says registered or remote
    /// access is turned off — so a box registered while no UI is polling
    /// still re-establishes.
    fn watch_registration(self: &Arc<Self>) {
        let mut inner = self.inner.lock().unwrap();
        let reg = &mut inner.registration;
        reg.watch_until = Some(Instant::now() + REGISTRATION_WATCH);
        if reg.watching {
            return;
        }
        reg.watching = true;
        drop(inner);
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let Some(this) = weak.upgrade() else { return };
                {
                    let mut inner = this.inner.lock().unwrap();
                    let reg = &mut inner.registration;
                    let done = reg.registered == Some(true)
                        || reg.watch_until.is_none_or(|t| Instant::now() >= t);
                    if done {
                        reg.watching = false;
                        reg.watch_until = None;
                        return;
                    }
                }
                let relay_host = this.settings().await.relay_host;
                this.poll_registration(&relay_host).await;
                drop(this);
                tokio::time::sleep(REGISTRATION_POLL_EVERY).await;
            }
        });
    }

    /// Tests: lift the poll rate limit for the next refresh.
    #[cfg(test)]
    pub(crate) fn forget_last_poll(&self) {
        self.inner.lock().unwrap().registration.last_poll = None;
    }

    /// Lease this loop's UDP port from the configured range, if any.
    fn lease_port(&self, key: &str, s: &RemoteAccessSettings) -> Option<PortLease> {
        let base = s.udp_port_base?;
        let lease = self.ports.lease(base, s.udp_port_count);
        if lease.is_none() {
            tracing::warn!(
                loop_key = %key,
                "remote access: UDP ports {base}..{} all in use; this device uses an \
                 ephemeral port (enlarge the range)",
                base as u32 + s.udp_port_count as u32 - 1
            );
        }
        lease
    }

    /// The credential a loop serves with right now, opened from the DB.
    /// `Ok(None)`: the rows no longer call for this plan (device revoked,
    /// or its state moved on — `reconcile` restarts the right loop).
    async fn load_credential(
        &self,
        id: &str,
        slot: Slot,
        p: Plan,
        identity: Option<&BoxIdentity>,
    ) -> Result<Option<BoxCredential>, LoadError> {
        let transient = |e: anyhow::Error| LoadError::Transient(e.to_string());
        let fatal = |e: anyhow::Error| LoadError::Fatal(e.to_string());
        let Some(row) = self.db.get_remote_device(id).await.map_err(transient)? else {
            return Ok(None);
        };
        let enr = self
            .db
            .get_remote_device_enrollment(id)
            .await
            .map_err(transient)?;
        let now = chrono::Utc::now().to_rfc3339();
        if plan(slot, enr.as_ref(), &now) != Some(p) {
            return Ok(None);
        }
        let identity_or = || {
            identity
                .cloned()
                .ok_or_else(|| LoadError::Fatal(IDENTITY_MISSING.into()))
        };
        let open_s = || secret::open(&self.vault_key, &row).map_err(fatal);
        let cred = match p {
            Plan::Legacy => BoxCredential::Legacy {
                s: open_s()?.pairing_secret(),
                identity: identity.cloned(),
            },
            Plan::LinkEnroll => BoxCredential::Link {
                s: open_s()?.pairing_secret(),
                identity: identity_or()?,
                mode: LinkMode::Enroll,
            },
            Plan::LinkRefuse(reason) => {
                let e = enr.as_ref().expect("plan requires a row");
                // Active rows hold S under the refuse-loop AAD; an expired
                // pending link still has it in the device row.
                let s = match (&e.link_secret_ciphertext, &e.link_secret_nonce) {
                    (Some(ct), Some(nonce)) if e.state == enrollment_state::ACTIVE => {
                        secret::open_link_refuse(&self.vault_key, id, ct, nonce).map_err(fatal)?
                    }
                    _ => open_s()?,
                };
                BoxCredential::Link {
                    s: s.pairing_secret(),
                    identity: identity_or()?,
                    mode: LinkMode::Refuse(reason),
                }
            }
            Plan::Enrolled => {
                let e = enr.as_ref().expect("plan requires a row");
                let (Some(ct), Some(nonce), Some(key)) = (
                    &e.rendezvous_ciphertext,
                    &e.rendezvous_nonce,
                    &e.device_pubkey,
                ) else {
                    return Err(LoadError::Fatal("enrollment row has no key or R".into()));
                };
                let device_key: [u8; 32] = key
                    .as_slice()
                    .try_into()
                    .map_err(|_| LoadError::Fatal("enrollment row: bad device key".into()))?;
                BoxCredential::Enrolled {
                    r: secret::open_rendezvous(&self.vault_key, id, ct, nonce).map_err(fatal)?,
                    identity: identity_or()?,
                    device_key,
                }
            }
        };
        Ok(Some(cred))
    }

    /// The activation transition for `device_id` (first handshake of the
    /// enrolled key on `rid(R)`): `S` moves to the refuse loop and the
    /// device row's secret becomes a tombstone, in one transaction. `Ok
    /// (false)` when the row isn't `staged`.
    async fn activate(&self, device_id: &str, device_key: [u8; 32]) -> anyhow::Result<bool> {
        let Some(enr) = self.db.get_remote_device_enrollment(device_id).await? else {
            return Ok(false);
        };
        if enr.state != enrollment_state::STAGED {
            return Ok(false);
        }
        if enr.device_pubkey.as_deref() != Some(device_key.as_slice()) {
            anyhow::bail!("activation by a key other than the enrolled one");
        }
        let Some(row) = self.db.get_remote_device(device_id).await? else {
            return Ok(false);
        };
        let s = secret::open(&self.vault_key, &row)?;
        let (link_secret_ciphertext, link_secret_nonce) =
            secret::seal_link_refuse(&self.vault_key, device_id, &s)?;
        let (tombstone_ciphertext, tombstone_nonce) =
            secret::seal(&self.vault_key, device_id, &DeviceSecret::generate())?;
        let now = chrono::Utc::now();
        let base = enr
            .link_expires_at
            .as_deref()
            .and_then(|e| chrono::DateTime::parse_from_rfc3339(e).ok())
            .map(|e| e.with_timezone(&chrono::Utc))
            .map_or(now, |e| e.max(now));
        let link_refuse_until =
            (base + chrono::Duration::from_std(LINK_REFUSE_GRACE)?).to_rfc3339();
        self.db
            .activate_remote_device_enrollment(ActivateAttempt {
                device_id: device_id.to_string(),
                now: now.to_rfc3339(),
                link_secret_ciphertext,
                link_secret_nonce,
                link_refuse_until,
                tombstone_ciphertext,
                tombstone_nonce,
            })
            .await
    }

    async fn device_loop(
        self: Arc<Self>,
        id: String,
        slot: Slot,
        p: Plan,
        settings: RemoteAccessSettings,
        app: Router,
        identity: Option<BoxIdentity>,
    ) {
        let key = slot.key(&id);
        let relay_host = settings.relay_host.clone();
        // Held for the loop's lifetime; dropped with the task on stop.
        let lease = p
            .leases_port()
            .then(|| self.lease_port(&key, &settings))
            .flatten();
        let public_host = (lease.is_some() && !settings.public_address.is_empty())
            .then(|| settings.public_address.clone());
        let public_ip: Arc<Mutex<Option<IpAddr>>> = Arc::default();
        let on_registered = {
            let this = Arc::downgrade(&self);
            let (key, public_ip) = (key.clone(), public_ip.clone());
            Arc::new(move |r: Registered| {
                *public_ip.lock().unwrap() = Some(r.public.ip());
                if let Some(this) = this.upgrade() {
                    this.set_registered(&key, r);
                }
            }) as tunnel::OnRegistered
        };
        let enroller: Arc<dyn EnrollHandler> = Arc::new(Enroller {
            ra: Arc::downgrade(&self),
            device_id: id.clone(),
            activated: AtomicBool::new(false),
        });
        let mut backoff = BACKOFF_MIN;
        loop {
            let cred = match self.load_credential(&id, slot, p, identity.as_ref()).await {
                Ok(Some(c)) => c,
                Ok(None) => return,
                Err(LoadError::Fatal(msg)) => {
                    // Not transient: the key or the row is wrong.
                    self.set_status(&key, DeviceStatus::error(msg));
                    return;
                }
                Err(LoadError::Transient(msg)) => {
                    self.set_status(&key, DeviceStatus::error(msg));
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                    continue;
                }
            };
            self.set_status(&key, DeviceStatus::waiting());
            let direct = DirectOptions {
                bind_port: lease.as_ref().map(PortLease::port),
                public_host: public_host.clone(),
                public_ip_hint: *public_ip.lock().unwrap(),
            };
            let punched = match self
                .backend
                .establish(
                    &relay_host,
                    &cred.relay_secret(),
                    &direct,
                    identity.as_ref(),
                    on_registered.clone(),
                )
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    tracing::debug!(device_id = %id, "remote access: establish failed: {e:#}");
                    self.set_status(&key, DeviceStatus::error(format!("{e:#}")));
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                    continue;
                }
            };
            backoff = BACKOFF_MIN;
            if let Some(s) = punched.relay_identity() {
                self.inner.lock().unwrap().registration.note_handshake(s);
            }
            // Punched (or relayed) only: the device isn't connected until
            // its QUIC handshake completes (`TunnelUpdate::Connected`).
            tracing::debug!(
                device_id = %id,
                path = punched.path(),
                cred = cred.kind(),
                "remote access: path established, awaiting device handshake"
            );
            let events = {
                let this = Arc::downgrade(&self);
                let (id, key) = (id.clone(), key.clone());
                Arc::new(move |u: TunnelUpdate| {
                    let Some(this) = this.upgrade() else { return };
                    match u {
                        TunnelUpdate::Connected { peer, rtt_ms, path } => {
                            tracing::info!(
                                device_id = %id,
                                path,
                                rtt_ms,
                                "remote access: device connected"
                            );
                            let (db, did) = (this.db.clone(), id.clone());
                            tokio::spawn(async move {
                                let now = chrono::Utc::now().to_rfc3339();
                                let _ = db.touch_remote_device(&did, &now).await;
                            });
                            this.set_status(
                                &key,
                                DeviceStatus {
                                    state: "connected",
                                    peer: Some(peer.to_string()),
                                    rtt_ms: Some(rtt_ms),
                                    path: Some(path),
                                    ..DeviceStatus::offline()
                                },
                            )
                        }
                        TunnelUpdate::PathChanged { path } => {
                            tracing::info!(device_id = %id, path, "remote access: tunnel path changed");
                            this.set_path(&key, path);
                        }
                        TunnelUpdate::Disconnected { .. } => {
                            this.set_status(&key, DeviceStatus::waiting())
                        }
                        TunnelUpdate::Error(e) => this.set_status(&key, DeviceStatus::error(e)),
                    }
                }) as tunnel::TunnelEvents
            };
            let res = tunnel::serve_tunnel(
                app.clone(),
                &id,
                punched,
                &cred,
                Some(enroller.clone()),
                events,
            )
            .await;
            tracing::info!(device_id = %id, cred = cred.kind(), "remote access: tunnel ended");
            if let Err(e) = res {
                self.set_status(&key, DeviceStatus::error(format!("{e:#}")));
                tokio::time::sleep(backoff).await;
            }
        }
    }
}

enum LoadError {
    /// Retry with backoff (DB busy).
    Transient(String),
    /// The loop can't run (wrong key, bad row, no identity): report and stop.
    Fatal(String),
}

/// The box side of pairing-v2 enrollment for one device, backed by the
/// CRUD transitions. One per device loop; the relay library calls it with
/// verified requests only.
struct Enroller {
    ra: Weak<RemoteAccess>,
    device_id: String,
    /// The row was seen active: later handshakes skip the DB.
    activated: AtomicBool,
}

#[async_trait]
impl EnrollHandler for Enroller {
    async fn enroll(
        &self,
        mode: EnrollMode,
        device_key: [u8; 32],
        name: &str,
        from: SocketAddr,
    ) -> Result<RendezvousSecret, RefuseReason> {
        let Some(ra) = self.ra.upgrade() else {
            return Err(RefuseReason::Internal);
        };
        let r = RendezvousSecret::generate();
        let (rendezvous_ciphertext, rendezvous_nonce) =
            secret::seal_rendezvous(&ra.vault_key, &self.device_id, &r).map_err(|e| {
                tracing::warn!(device_id = %self.device_id, "remote access: sealing R: {e}");
                RefuseReason::Internal
            })?;
        let outcome = ra
            .db
            .enroll_remote_device(EnrollAttempt {
                device_id: self.device_id.clone(),
                legacy_upgrade: mode == EnrollMode::LegacyUpgrade,
                device_pubkey: device_key,
                rendezvous_ciphertext,
                rendezvous_nonce,
                device_name_hint: name.to_string(),
                from: from.to_string(),
                now: chrono::Utc::now().to_rfc3339(),
            })
            .await
            .map_err(|e| {
                tracing::warn!(device_id = %self.device_id, "remote access: enrollment failed: {e}");
                RefuseReason::Internal
            })?;
        match outcome {
            EnrollOutcome::Granted => {
                tracing::info!(
                    device_id = %self.device_id,
                    %from,
                    legacy_upgrade = mode == EnrollMode::LegacyUpgrade,
                    device = %peckboard_relay::identity::fingerprint(&device_key),
                    "remote access: device enrolled"
                );
                // Start its rid(R) loop.
                ra.reconcile_soon();
                Ok(r)
            }
            EnrollOutcome::ReDelivered {
                rendezvous_ciphertext,
                rendezvous_nonce,
            } => secret::open_rendezvous(
                &ra.vault_key,
                &self.device_id,
                &rendezvous_ciphertext,
                &rendezvous_nonce,
            )
            .map_err(|_| RefuseReason::Internal),
            EnrollOutcome::Refused(why) => {
                tracing::warn!(device_id = %self.device_id, %from, ?why, "remote access: enrollment refused");
                Err(match why {
                    EnrollRefusal::AlreadyUsed => RefuseReason::AlreadyUsed,
                    EnrollRefusal::Expired => RefuseReason::Expired,
                    EnrollRefusal::NotEnrollable => RefuseReason::NotEnrollable,
                })
            }
        }
    }

    async fn link_reuse(&self, from: SocketAddr, reason: RefuseReason) {
        let Some(ra) = self.ra.upgrade() else { return };
        tracing::warn!(device_id = %self.device_id, %from, reason = reason.as_str(), "remote access: used pairing link contacted");
        let now = chrono::Utc::now().to_rfc3339();
        if let Err(e) = ra
            .db
            .note_remote_link_reuse(&self.device_id, &from.to_string(), &now)
            .await
        {
            tracing::warn!(device_id = %self.device_id, "remote access: noting link reuse: {e}");
        }
    }

    async fn activated(&self, device_key: [u8; 32], from: SocketAddr) {
        // Fires on every accepted rid(R) handshake; only the first does
        // work, and `Connected` already stamps `last_connected_at`.
        if self.activated.load(Ordering::SeqCst) {
            return;
        }
        let Some(ra) = self.ra.upgrade() else { return };
        match ra.activate(&self.device_id, device_key).await {
            Ok(done) => {
                self.activated.store(true, Ordering::SeqCst);
                if done {
                    tracing::info!(device_id = %self.device_id, %from, "remote access: pairing activated; link secret retired");
                    // The rid(S) loop switches to refusing.
                    ra.reconcile_soon();
                }
            }
            Err(e) => {
                tracing::warn!(device_id = %self.device_id, "remote access: activation failed: {e}")
            }
        }
    }
}

/// Fakes for the service and route tests: a backend that connects every
/// pairing at once to a fake peer and plays a relay whose registry is a
/// flag. `serve` blocks until the test drops the loop.
#[cfg(test)]
pub(crate) mod testing {
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use peckboard_relay::keys::PairingSecret;

    use super::tunnel::{
        BoxCredential, BoxIdentity, DirectOptions, EnrollHandler, IdentityStatus, OnRegistered,
        PunchedTunnel, Registered, TunnelBackend, TunnelEvents, TunnelUpdate,
    };

    #[derive(Default)]
    pub(crate) struct FakeBackend {
        /// The port each registration bound.
        pub(crate) bound: Mutex<Vec<Option<u16>>>,
        /// The identity key handed to each `establish`.
        pub(crate) identities: Mutex<Vec<Option<[u8; 32]>>>,
        /// The credential kind of every served tunnel, in order.
        pub(crate) served: Arc<Mutex<Vec<&'static str>>>,
        /// The relay's registration gate, as its handshake reports it.
        pub(crate) gated: AtomicBool,
        /// The relay's registry: is the box's key in it?
        pub(crate) registered: AtomicBool,
        /// A relay that predates relay registration: no verdict in the
        /// handshake, status requests fail.
        pub(crate) legacy_relay: AtomicBool,
        pub(crate) polls: AtomicUsize,
    }
    pub(crate) struct FakePunched {
        served: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait::async_trait]
    impl TunnelBackend for FakeBackend {
        async fn establish(
            &self,
            _relay_host: &str,
            _secret: &PairingSecret,
            direct: &DirectOptions,
            identity: Option<&BoxIdentity>,
            on_registered: OnRegistered,
        ) -> anyhow::Result<Box<dyn PunchedTunnel>> {
            self.bound.lock().unwrap().push(direct.bind_port);
            self.identities
                .lock()
                .unwrap()
                .push(identity.map(|i| i.public_key()));
            let port = direct.bind_port.unwrap_or(50000);
            let public: SocketAddr = format!("203.0.113.5:{port}").parse().unwrap();
            let verdict = (!self.legacy_relay.load(Ordering::SeqCst)).then(|| IdentityStatus {
                registered: identity.is_some() && self.registered.load(Ordering::SeqCst),
                gated: self.gated.load(Ordering::SeqCst),
            });
            on_registered(Registered {
                local_port: port,
                public,
                candidates: vec![public],
                identity: verdict,
            });
            Ok(Box::new(FakePunched {
                served: self.served.clone(),
            }))
        }

        async fn registration_status(
            &self,
            _relay_host: &str,
            _key: &[u8; 32],
        ) -> anyhow::Result<bool> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            if self.legacy_relay.load(Ordering::SeqCst) {
                anyhow::bail!("relay answered 404 Not Found");
            }
            Ok(self.registered.load(Ordering::SeqCst))
        }
    }

    #[async_trait::async_trait]
    impl PunchedTunnel for FakePunched {
        fn peer(&self) -> SocketAddr {
            "198.51.100.7:4444".parse().unwrap()
        }
        async fn serve(
            self: Box<Self>,
            cred: &BoxCredential,
            _target: SocketAddr,
            _enroll: Option<Arc<dyn EnrollHandler>>,
            events: TunnelEvents,
        ) -> anyhow::Result<()> {
            self.served.lock().unwrap().push(cred.kind());
            events(TunnelUpdate::Connected {
                peer: self.peer(),
                rtt_ms: 1,
                path: "direct",
            });
            std::future::pending().await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeBackend;
    use super::*;
    use crate::db::models::NewRemoteDevice;
    use std::sync::atomic::Ordering::SeqCst;
    #[test]
    fn relay_host_validation() {
        assert!(validate_relay_host("relay.peckboard.com").is_ok());
        assert!(validate_relay_host("127.0.0.1:24430").is_ok());
        for bad in ["", "a b", "x:0", "x:99999", "h/path", "user@h"] {
            assert!(validate_relay_host(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn direct_validation() {
        assert!(validate_direct(None, 10, "").is_ok());
        let ok = validate_direct(Some(40000), 10, " home.example.com ").unwrap();
        assert_eq!(ok.public_address, "home.example.com");
        assert!(validate_direct(Some(40000), 1, "203.0.113.5").is_ok());
        assert!(validate_direct(Some(40000), 1, "2001:db8::1").is_ok());
        for (base, count, public, field) in [
            (Some(80), 10, "", "udp_port_base"),
            (Some(70000), 10, "", "udp_port_base"),
            (Some(40000), 0, "", "udp_port_count"),
            (Some(40000), 300, "", "udp_port_count"),
            (Some(65530), 10, "", "udp_port_count"),
            (Some(40000), 10, "1.2.3.4:5", "public_address"),
            (Some(40000), 10, "bad host", "public_address"),
            (None, 10, "1.2.3.4", "public_address"),
        ] {
            assert_eq!(
                validate_direct(base, count, public).unwrap_err().0,
                field,
                "{base:?} {count} {public}"
            );
        }
    }

    #[test]
    fn port_pool_leases_lowest_free_and_releases() {
        let pool = PortPool::default();
        let a = pool.lease(40000, 3).unwrap();
        let b = pool.lease(40000, 3).unwrap();
        let c = pool.lease(40000, 3).unwrap();
        assert_eq!((a.port(), b.port(), c.port()), (40000, 40001, 40002));
        assert!(pool.lease(40000, 3).is_none(), "range exhausted");
        drop(b);
        assert_eq!(pool.lease(40000, 3).unwrap().port(), 40001);
        assert_eq!(pool.lease(65535, 5).unwrap().port(), 65535);
    }

    fn new_device(id: &str) -> NewRemoteDevice {
        NewRemoteDevice {
            id: id.into(),
            user_id: "u1".into(),
            name: "phone".into(),
            secret_ciphertext: Vec::new(),
            secret_nonce: Vec::new(),
            created_at: chrono::Utc::now().to_rfc3339(),
            last_connected_at: None,
        }
    }

    /// A legacy (S-only) pairing.
    async fn pair(db: &Db, key: &[u8], id: &str) -> DeviceSecret {
        let s = DeviceSecret::generate();
        let (ct, nonce) = secret::seal(key, id, &s).unwrap();
        db.insert_remote_device(NewRemoteDevice {
            secret_ciphertext: ct,
            secret_nonce: nonce,
            ..new_device(id)
        })
        .await
        .unwrap();
        s
    }

    /// A v2 link expiring `expires_in` seconds from now.
    async fn pair_v2(db: &Db, key: &[u8], id: &str, expires_in: i64) -> DeviceSecret {
        let s = DeviceSecret::generate();
        let (ct, nonce) = secret::seal(key, id, &s).unwrap();
        let exp = (chrono::Utc::now() + chrono::Duration::seconds(expires_in)).to_rfc3339();
        db.insert_remote_device_with_link(
            NewRemoteDevice {
                secret_ciphertext: ct,
                secret_nonce: nonce,
                ..new_device(id)
            },
            exp,
        )
        .await
        .unwrap();
        s
    }

    async fn wait_connected(ra: &RemoteAccess, id: &str) {
        for _ in 0..100 {
            if ra.status(id).state == "connected" {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{id} never connected: {:?}", ra.status(id));
    }

    async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// The running loops as `(key, plan)`, sorted.
    fn plans(ra: &RemoteAccess) -> Vec<(String, Plan)> {
        let inner = ra.inner.lock().unwrap();
        let mut v: Vec<_> = inner
            .tasks
            .iter()
            .map(|(k, t)| (k.clone(), t.plan))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    #[tokio::test]
    async fn loops_follow_setting_and_revoke() {
        let db = Db::in_memory().unwrap();
        let key = vec![3u8; 32];
        pair(&db, &key, "d1").await;
        let dir = tempfile::tempdir().unwrap();
        let ra = RemoteAccess::new(
            db.clone(),
            key,
            dir.path(),
            Arc::new(FakeBackend::default()),
        );
        ra.start(Router::new()).await;
        assert_eq!(ra.status("d1").state, "offline", "default off");

        ra.put_settings(&RemoteAccessSettings {
            enabled: true,
            ..Default::default()
        })
        .await
        .unwrap();
        wait_connected(&ra, "d1").await;
        let st = ra.status("d1");
        assert_eq!(st.peer.as_deref(), Some("198.51.100.7:4444"));
        let row = db.get_remote_device("d1").await.unwrap().unwrap();
        assert!(row.last_connected_at.is_some());

        ra.stop_device("d1");
        assert_eq!(ra.status("d1").state, "offline");
        assert!(ra.inner.lock().unwrap().tasks.is_empty());
    }

    /// Each device loop binds the lowest free port of the range; once the
    /// range is exhausted the next one falls back to an ephemeral port, and
    /// a revoked device's port is free again.
    #[tokio::test]
    async fn device_loops_lease_ports_from_the_range() {
        let db = Db::in_memory().unwrap();
        let key = vec![3u8; 32];
        pair(&db, &key, "d1").await;
        pair(&db, &key, "d2").await;
        let backend = Arc::new(FakeBackend::default());
        let dir = tempfile::tempdir().unwrap();
        let ra = RemoteAccess::new(db.clone(), key.clone(), dir.path(), backend.clone());
        ra.start(Router::new()).await;
        ra.put_settings(&RemoteAccessSettings {
            enabled: true,
            udp_port_base: Some(40000),
            udp_port_count: 1,
            ..Default::default()
        })
        .await
        .unwrap();
        wait_connected(&ra, "d1").await;
        wait_connected(&ra, "d2").await;
        let mut bound = backend.bound.lock().unwrap().clone();
        bound.sort();
        assert_eq!(bound, vec![None, Some(40000)]);
        let (holder, other) = if ra.status("d1").local_port == Some(40000) {
            ("d1", "d2")
        } else {
            ("d2", "d1")
        };
        assert_eq!(ra.status(holder).candidates, vec!["203.0.113.5:40000"]);
        assert_eq!(ra.status(other).local_port, Some(50000), "ephemeral");

        ra.stop_device(holder);
        tokio::task::yield_now().await;
        db.delete_remote_device(holder).await.unwrap();
        pair(&db, &key, "d3").await;
        ra.reconcile().await;
        wait_connected(&ra, "d3").await;
        assert_eq!(
            ra.status("d3").local_port,
            Some(40000),
            "released on revoke"
        );
    }

    /// Pairing v2 across the state machine, as `reconcile` sees it: a
    /// pending link runs one enrollment-only `rid(S)` loop (no port lease),
    /// an expired one a refusing loop, a long-expired one nothing; an
    /// enrollment adds the `rid(R)` loop; activation retires S (tombstone +
    /// refuse loop) and the refuse loop ends with its grace period.
    #[tokio::test]
    async fn reconcile_runs_the_loops_each_enrollment_state_calls_for() {
        let db = Db::in_memory().unwrap();
        let key = vec![3u8; 32];
        let backend = Arc::new(FakeBackend::default());
        let dir = tempfile::tempdir().unwrap();
        let ra = RemoteAccess::new(db.clone(), key.clone(), dir.path(), backend.clone());
        // Issuing the first link creates the identity the links pin.
        ra.link_identity().await.unwrap();
        pair(&db, &key, "legacy").await;
        let s = pair_v2(&db, &key, "pending", 3600).await;
        pair_v2(&db, &key, "expired", -5).await;
        pair_v2(&db, &key, "ancient", -3 * 86_400).await;
        ra.start(Router::new()).await;
        ra.put_settings(&RemoteAccessSettings {
            enabled: true,
            udp_port_base: Some(40000),
            udp_port_count: 4,
            ..Default::default()
        })
        .await
        .unwrap();
        assert_eq!(
            plans(&ra),
            vec![
                ("expired/s".into(), Plan::LinkRefuse(RefuseReason::Expired)),
                ("legacy/s".into(), Plan::Legacy),
                ("pending/s".into(), Plan::LinkEnroll),
            ]
        );
        wait_connected(&ra, "legacy").await;
        wait_connected(&ra, "pending").await;
        assert_eq!(ra.status("legacy").local_port, Some(40000), "legacy leases");
        assert_eq!(
            ra.status("pending").local_port,
            Some(50000),
            "link loops don't"
        );
        assert_eq!(ra.status("ancient").state, "offline");

        // The device enrolls over the link loop.
        let enroller = Enroller {
            ra: Arc::downgrade(&ra),
            device_id: "pending".into(),
            activated: AtomicBool::new(false),
        };
        let from: SocketAddr = "203.0.113.7:4000".parse().unwrap();
        let r = enroller
            .enroll(EnrollMode::Link, [7; 32], "iPhone", from)
            .await
            .unwrap();
        let again = enroller
            .enroll(EnrollMode::Link, [7; 32], "iPhone", from)
            .await
            .unwrap();
        assert!(again == r, "same key, same R");
        assert_eq!(
            enroller
                .enroll(EnrollMode::Link, [8; 32], "other", from)
                .await
                .err()
                .unwrap(),
            RefuseReason::AlreadyUsed
        );
        wait_for("the R loop", || {
            plans(&ra).contains(&("pending/r".into(), Plan::Enrolled))
        })
        .await;
        assert!(plans(&ra).contains(&("pending/s".into(), Plan::LinkEnroll)));
        wait_connected(&ra, "pending").await;
        assert_eq!(
            ra.status("pending").local_port,
            Some(40001),
            "enrolled leases"
        );
        let enr = db
            .get_remote_device_enrollment("pending")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(enr.state, "staged");
        assert_eq!(enr.reuse_attempts, 1);
        assert_eq!(
            enrollment_view_state(Some(&enr), &chrono::Utc::now().to_rfc3339()),
            "staged"
        );

        // First handshake on rid(R): active, S tombstoned and moved.
        enroller.activated([7; 32], from).await;
        enroller.activated([7; 32], from).await;
        let enr = db
            .get_remote_device_enrollment("pending")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(enr.state, "active");
        let row = db.get_remote_device("pending").await.unwrap().unwrap();
        assert_ne!(
            secret::open(&key, &row).unwrap().as_bytes(),
            s.as_bytes(),
            "device row no longer opens to S"
        );
        let moved = secret::open_link_refuse(
            &key,
            "pending",
            enr.link_secret_ciphertext.as_ref().unwrap(),
            enr.link_secret_nonce.as_ref().unwrap(),
        )
        .unwrap();
        assert_eq!(moved.as_bytes(), s.as_bytes());
        wait_for("the S loop to refuse", || {
            plans(&ra).contains(&(
                "pending/s".into(),
                Plan::LinkRefuse(RefuseReason::AlreadyUsed),
            ))
        })
        .await;
        assert!(backend.served.lock().unwrap().contains(&"enrolled"));

        // Past the grace period the refuse loop goes; R stays.
        let far = (chrono::Utc::now() + chrono::Duration::days(2)).to_rfc3339();
        assert_eq!(db.clear_expired_remote_link_secrets(&far).await.unwrap(), 1);
        ra.reconcile().await;
        assert_eq!(
            plans(&ra)
                .into_iter()
                .filter(|(k, _)| k.starts_with("pending/"))
                .collect::<Vec<_>>(),
            vec![("pending/r".into(), Plan::Enrolled)]
        );

        // Revoke stops both.
        ra.stop_device("pending");
        assert!(plans(&ra).iter().all(|(k, _)| !k.starts_with("pending/")));
    }

    /// SECURITY: once a device pins the box identity, a missing identity
    /// file is never silently replaced — the enrolled device shows an
    /// error instead, legacy devices keep running, and a new link can't be
    /// issued until the file is back or every enrolled device is gone.
    #[tokio::test]
    async fn identity_is_never_recreated_once_a_device_pins_it() {
        let db = Db::in_memory().unwrap();
        let key = vec![3u8; 32];
        pair(&db, &key, "legacy").await;
        pair_v2(&db, &key, "linked", 3600).await;
        let backend = Arc::new(FakeBackend::default());
        let dir = tempfile::tempdir().unwrap();
        let identity_file = dir.path().join(IDENTITY_FILE);
        let ra = RemoteAccess::new(db.clone(), key.clone(), dir.path(), backend.clone());
        ra.start(Router::new()).await;
        // A pending link already pins B, so the file isn't created now…
        ra.put_settings(&RemoteAccessSettings {
            enabled: true,
            ..Default::default()
        })
        .await
        .unwrap();
        assert!(!identity_file.exists());
        assert_eq!(ra.status("linked").state, "error");
        assert_eq!(ra.status("linked").error.as_deref(), Some(IDENTITY_MISSING));
        wait_connected(&ra, "legacy").await;
        assert!(ra.link_identity().await.is_err());
        // …until the pinning row is gone.
        db.delete_remote_device("linked").await.unwrap();
        let id = ra.link_identity().await.unwrap();
        assert!(identity_file.exists());
        assert_eq!(ra.box_fingerprint(), Some(id.fingerprint()));
        pair_v2(&db, &key, "linked2", 3600).await;
        ra.reconcile().await;
        wait_connected(&ra, "linked2").await;

        // The file disappears on a running box: nothing is regenerated.
        std::fs::remove_file(&identity_file).unwrap();
        *ra.identity.lock().unwrap() = None;
        ra.stop_all();
        ra.reconcile().await;
        assert!(!identity_file.exists());
        assert_eq!(
            ra.status("linked2").error.as_deref(),
            Some(IDENTITY_MISSING)
        );
        wait_connected(&ra, "legacy").await;
        assert!(ra.link_identity().await.is_err());
    }

    /// Enabling creates the box identity and hands it to every establish;
    /// the relay's handshake verdict and status polls feed `registration`;
    /// once a poll says the admin registered, the loops re-establish and
    /// carry the new verdict. A relay that predates relay registration
    /// yields nothing to report.
    #[tokio::test]
    async fn relay_registration_tracks_polls_and_reestablishes() {
        let dir = tempfile::tempdir().unwrap();
        let identity_file = dir.path().join(IDENTITY_FILE);
        let db = Db::in_memory().unwrap();
        let key = vec![3u8; 32];
        pair(&db, &key, "d1").await;
        let backend = Arc::new(FakeBackend::default());
        backend.gated.store(true, SeqCst);
        let ra = RemoteAccess::new(db.clone(), key, dir.path(), backend.clone());
        ra.start(Router::new()).await;
        assert!(!identity_file.exists(), "no identity until enabled");
        assert_eq!(
            ra.registration("relay.test"),
            RegistrationView {
                supported: false,
                registered: None,
                gated: None,
                url: String::new(),
            }
        );

        ra.put_settings(&RemoteAccessSettings {
            enabled: true,
            relay_host: "relay.test".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        wait_connected(&ra, "d1").await;
        let id = BoxIdentity::load_or_create(&identity_file).unwrap();
        assert_eq!(
            backend.identities.lock().unwrap().as_slice(),
            &[Some(id.public_key())]
        );
        let reg = ra.registration("relay.test");
        assert_eq!(
            (reg.supported, reg.registered, reg.gated),
            (true, Some(false), Some(true))
        );
        assert_eq!(
            reg.url,
            format!("https://relay.test/register#{}", id.public_key_b64())
        );
        // Enabling started a background watch; its first poll went out.
        wait_for("the watch to poll", || backend.polls.load(SeqCst) >= 1).await;
        assert!(ra.inner.lock().unwrap().registration.watching);

        // Still unregistered: refresh says so and stays rate-limited.
        let polls = backend.polls.load(SeqCst);
        assert_eq!(ra.refresh_registration().await.registered, Some(false));
        assert_eq!(backend.polls.load(SeqCst), polls, "polled within 5 s");

        // The admin registers on the relay: the next poll notices and the
        // loop re-establishes with the new verdict.
        backend.registered.store(true, SeqCst);
        ra.forget_last_poll();
        assert_eq!(ra.refresh_registration().await.registered, Some(true));
        wait_for("re-establish", || {
            backend.identities.lock().unwrap().len() == 2
        })
        .await;
        wait_connected(&ra, "d1").await;
        let reg = ra.registration("relay.test");
        assert_eq!((reg.registered, reg.gated), (Some(true), Some(true)));
        assert_eq!(
            ra.inner
                .lock()
                .unwrap()
                .registration
                .handshake
                .map(|h| h.registered),
            Some(true)
        );

        // Another relay, one that predates relay registration: the state
        // resets with the host, the handshake carries no verdict and the
        // status request fails — nothing to report, but the same identity.
        backend.legacy_relay.store(true, SeqCst);
        ra.put_settings(&RemoteAccessSettings {
            enabled: true,
            relay_host: "old.relay.test".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        wait_connected(&ra, "d1").await;
        ra.forget_last_poll();
        let reg = ra.refresh_registration().await;
        assert_eq!(
            (reg.supported, reg.registered, reg.gated),
            (false, None, None)
        );
        assert_eq!(
            reg.url,
            format!("https://old.relay.test/register#{}", id.public_key_b64())
        );

        // Off: the watch winds down.
        ra.put_settings(&RemoteAccessSettings::default())
            .await
            .unwrap();
        assert!(ra.inner.lock().unwrap().registration.watch_until.is_none());
    }
}
