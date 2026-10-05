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
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use axum::Router;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use crate::db::Db;
use crate::routes::settings::{SETTINGS_COLLECTION, SETTINGS_NS};
use secret::DeviceSecret;
use tunnel::{BoxIdentity, DirectOptions, IdentityStatus, Registered, TunnelBackend, TunnelUpdate};

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

#[derive(Default)]
struct Inner {
    tasks: HashMap<String, JoinHandle<()>>,
    status: HashMap<String, DeviceStatus>,
    /// The last registration per device, merged into its status.
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
    /// loop the stored setting calls for.
    pub async fn start(self: &Arc<Self>, app: Router) {
        let _ = self.app.set(app);
        self.reconcile().await;
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

    pub fn status(&self, device_id: &str) -> DeviceStatus {
        let inner = self.inner.lock().unwrap();
        let mut s = inner
            .status
            .get(device_id)
            .cloned()
            .unwrap_or_else(DeviceStatus::offline);
        if let Some(r) = inner.registered.get(device_id) {
            s.local_port = Some(r.local_port);
            s.candidates = r.candidates.iter().map(|a| a.to_string()).collect();
        }
        s
    }

    fn set_status(&self, device_id: &str, s: DeviceStatus) {
        let mut inner = self.inner.lock().unwrap();
        // A revoked/stopped device's loop may race one last update in.
        if inner.tasks.contains_key(device_id) {
            inner.status.insert(device_id.to_string(), s);
        }
    }

    /// A connected tunnel moved between the direct and relayed path.
    fn set_path(&self, device_id: &str, path: &'static str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(s) = inner.status.get_mut(device_id)
            && s.state == "connected"
        {
            s.path = Some(path);
        }
    }

    fn set_registered(&self, device_id: &str, r: Registered) {
        let mut inner = self.inner.lock().unwrap();
        if inner.tasks.contains_key(device_id) {
            if let Some(s) = r.identity {
                inner.registration.note_handshake(s);
            }
            inner.registered.insert(device_id.to_string(), r);
        }
    }

    /// Stop one device's loop and drop its tunnel (every open connection
    /// through it dies with the task, and its UDP port lease with it).
    pub fn stop_device(&self, device_id: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(h) = inner.tasks.remove(device_id) {
            h.abort();
        }
        inner.status.remove(device_id);
        inner.registered.remove(device_id);
    }

    fn stop_all(&self) {
        let mut inner = self.inner.lock().unwrap();
        for (_, h) in inner.tasks.drain() {
            h.abort();
        }
        inner.status.clear();
        inner.registered.clear();
        // The sessions that verdict described are gone.
        inner.registration.handshake = None;
    }

    /// Bring running loops in line with the setting and the DB: none when
    /// disabled, exactly one per paired device when enabled.
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
        // Created on first enable: the relay may require a registered box
        // for the relayed fallback. Without one, loops still run (direct
        // paths never need it).
        if let Err(e) = self.identity(true) {
            tracing::warn!("remote access: {e:#}; relay registration unavailable");
        }
        let devices = match self.db.list_remote_devices().await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("remote access: listing devices failed: {e}");
                return;
            }
        };
        let mut inner = self.inner.lock().unwrap();
        let wanted: std::collections::HashSet<&str> =
            devices.iter().map(|d| d.id.as_str()).collect();
        inner.tasks.retain(|id, h| {
            let keep = wanted.contains(id.as_str()) && !h.is_finished();
            if !keep {
                h.abort();
            }
            keep
        });
        for d in &devices {
            if inner.tasks.contains_key(&d.id) {
                continue;
            }
            let this = self.clone();
            let id = d.id.clone();
            let settings = settings.clone();
            let app = app.clone();
            inner.status.insert(d.id.clone(), DeviceStatus::waiting());
            inner.tasks.insert(
                d.id.clone(),
                tokio::spawn(async move { this.device_loop(id, settings, app).await }),
            );
        }
        drop(inner);
        self.watch_registration();
    }

    /// The box's identity key: cached, else loaded from `identity_path`,
    /// else — when `create` — generated there. `Ok(None)`: no file yet.
    fn identity(&self, create: bool) -> anyhow::Result<Option<BoxIdentity>> {
        let mut slot = self.identity.lock().unwrap();
        if let Some(id) = &*slot {
            return Ok(Some(id.clone()));
        }
        if !create && !self.identity_path.exists() {
            return Ok(None);
        }
        let id = BoxIdentity::load_or_create(&self.identity_path)
            .with_context(|| format!("box identity {}", self.identity_path.display()))?;
        *slot = Some(id.clone());
        Ok(Some(id))
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
        if let Err(e) = self.identity(true) {
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
    fn lease_port(&self, id: &str, s: &RemoteAccessSettings) -> Option<PortLease> {
        let base = s.udp_port_base?;
        let lease = self.ports.lease(base, s.udp_port_count);
        if lease.is_none() {
            tracing::warn!(
                device_id = %id,
                "remote access: UDP ports {base}..{} all in use; this device uses an \
                 ephemeral port (enlarge the range)",
                base as u32 + s.udp_port_count as u32 - 1
            );
        }
        lease
    }

    async fn device_loop(self: Arc<Self>, id: String, settings: RemoteAccessSettings, app: Router) {
        let relay_host = settings.relay_host.clone();
        // Created by `reconcile`; a loop runs without one if that failed.
        let identity = self.identity(false).ok().flatten();
        // Held for the loop's lifetime; dropped with the task on stop.
        let lease = self.lease_port(&id, &settings);
        let public_host = (lease.is_some() && !settings.public_address.is_empty())
            .then(|| settings.public_address.clone());
        let public_ip: Arc<Mutex<Option<IpAddr>>> = Arc::default();
        let on_registered = {
            let this = Arc::downgrade(&self);
            let (id, public_ip) = (id.clone(), public_ip.clone());
            Arc::new(move |r: Registered| {
                *public_ip.lock().unwrap() = Some(r.public.ip());
                if let Some(this) = this.upgrade() {
                    this.set_registered(&id, r);
                }
            }) as tunnel::OnRegistered
        };
        let mut backoff = BACKOFF_MIN;
        loop {
            let secret: DeviceSecret = match self.db.get_remote_device(&id).await {
                Ok(Some(row)) => match secret::open(&self.vault_key, &row) {
                    Ok(s) => s,
                    Err(e) => {
                        // Not transient: the key or the row is wrong.
                        self.set_status(&id, DeviceStatus::error(e.to_string()));
                        return;
                    }
                },
                Ok(None) => return,
                Err(e) => {
                    self.set_status(&id, DeviceStatus::error(e.to_string()));
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                    continue;
                }
            };
            self.set_status(&id, DeviceStatus::waiting());
            let direct = DirectOptions {
                bind_port: lease.as_ref().map(PortLease::port),
                public_host: public_host.clone(),
                public_ip_hint: *public_ip.lock().unwrap(),
            };
            let punched = match self
                .backend
                .establish(
                    &relay_host,
                    &secret,
                    &direct,
                    identity.as_ref(),
                    on_registered.clone(),
                )
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    tracing::debug!(device_id = %id, "remote access: establish failed: {e:#}");
                    self.set_status(&id, DeviceStatus::error(format!("{e:#}")));
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
                "remote access: path established, awaiting device handshake"
            );
            let events = {
                let this = Arc::downgrade(&self);
                let id = id.clone();
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
                                &id,
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
                            this.set_path(&id, path);
                        }
                        TunnelUpdate::Disconnected { .. } => {
                            this.set_status(&id, DeviceStatus::waiting())
                        }
                        TunnelUpdate::Error(e) => this.set_status(&id, DeviceStatus::error(e)),
                    }
                }) as tunnel::TunnelEvents
            };
            let res = tunnel::serve_tunnel(app.clone(), &id, punched, &secret, events).await;
            tracing::info!(device_id = %id, "remote access: tunnel ended");
            if let Err(e) = res {
                self.set_status(&id, DeviceStatus::error(format!("{e:#}")));
                tokio::time::sleep(backoff).await;
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
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::secret::DeviceSecret;
    use super::tunnel::{
        BoxIdentity, DirectOptions, IdentityStatus, OnRegistered, PunchedTunnel, Registered,
        TunnelBackend, TunnelEvents, TunnelUpdate,
    };

    #[derive(Default)]
    pub(crate) struct FakeBackend {
        /// The port each registration bound.
        pub(crate) bound: Mutex<Vec<Option<u16>>>,
        /// The identity key handed to each `establish`.
        pub(crate) identities: Mutex<Vec<Option<[u8; 32]>>>,
        /// The relay's registration gate, as its handshake reports it.
        pub(crate) gated: AtomicBool,
        /// The relay's registry: is the box's key in it?
        pub(crate) registered: AtomicBool,
        /// A relay that predates relay registration: no verdict in the
        /// handshake, status requests fail.
        pub(crate) legacy_relay: AtomicBool,
        pub(crate) polls: AtomicUsize,
    }
    pub(crate) struct FakePunched;

    #[async_trait::async_trait]
    impl TunnelBackend for FakeBackend {
        async fn establish(
            &self,
            _relay_host: &str,
            _secret: &DeviceSecret,
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
            Ok(Box::new(FakePunched))
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
            _secret: &DeviceSecret,
            _target: SocketAddr,
            events: TunnelEvents,
        ) -> anyhow::Result<()> {
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

    async fn pair(db: &Db, key: &[u8], id: &str) {
        let s = DeviceSecret::generate();
        let (ct, nonce) = secret::seal(key, id, &s).unwrap();
        db.insert_remote_device(NewRemoteDevice {
            id: id.into(),
            user_id: "u1".into(),
            name: "phone".into(),
            secret_ciphertext: ct,
            secret_nonce: nonce,
            created_at: chrono::Utc::now().to_rfc3339(),
            last_connected_at: None,
        })
        .await
        .unwrap();
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
