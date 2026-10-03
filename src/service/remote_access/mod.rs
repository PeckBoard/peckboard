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
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::Router;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use crate::db::Db;
use crate::routes::settings::{SETTINGS_COLLECTION, SETTINGS_NS};
use secret::DeviceSecret;
use tunnel::{DirectOptions, Registered, TunnelBackend, TunnelUpdate};

const SETTINGS_KEY: &str = "remote_access";
pub const DEFAULT_RELAY_HOST: &str = "relay.peckboard.com";
pub const DEFAULT_UDP_PORT_COUNT: u16 = 10;
pub const MAX_UDP_PORT_COUNT: u16 = 256;
const BACKOFF_MIN: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

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

#[derive(Default)]
struct Inner {
    tasks: HashMap<String, JoinHandle<()>>,
    status: HashMap<String, DeviceStatus>,
    /// The last registration per device, merged into its status.
    registered: HashMap<String, Registered>,
}

pub struct RemoteAccess {
    db: Db,
    vault_key: Vec<u8>,
    backend: Arc<dyn TunnelBackend>,
    /// Peckboard's router, bound once the server has built it.
    app: OnceLock<Router>,
    inner: Mutex<Inner>,
    ports: PortPool,
}

impl RemoteAccess {
    pub fn new(db: Db, vault_key: Vec<u8>, backend: Arc<dyn TunnelBackend>) -> Arc<Self> {
        Arc::new(Self {
            db,
            vault_key,
            backend,
            app: OnceLock::new(),
            inner: Mutex::new(Inner::default()),
            ports: PortPool::default(),
        })
    }

    /// A manager over its own empty in-memory DB that is never started, so
    /// it never contacts a relay — for test harnesses building `AppState`.
    pub fn inert() -> Arc<Self> {
        Self::new(
            Db::in_memory().expect("in-memory db"),
            vec![0u8; 32],
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
        let db = self.db.clone();
        let value = serde_json::to_string(s)?;
        tokio::task::spawn_blocking(move || {
            db.plugin_store_put_blocking(SETTINGS_NS, SETTINGS_COLLECTION, SETTINGS_KEY, &value)
        })
        .await??;
        self.stop_all();
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
            return;
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
                .establish(&relay_host, &secret, &direct, on_registered.clone())
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
            let peer = punched.peer().to_string();
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
                        TunnelUpdate::Connected { rtt_ms, path } => {
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
                                    peer: Some(peer.clone()),
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
            let res = tunnel::serve_tunnel(app.clone(), punched, &secret, events).await;
            tracing::info!(device_id = %id, "remote access: tunnel ended");
            if let Err(e) = res {
                self.set_status(&id, DeviceStatus::error(format!("{e:#}")));
                tokio::time::sleep(backoff).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::NewRemoteDevice;
    use std::net::SocketAddr;
    use tunnel::{OnRegistered, PunchedTunnel, TunnelEvents};

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

    /// Connects every pairing at once to a fake peer; `serve` blocks until
    /// the test drops the loop. Records the port each registration bound.
    #[derive(Default)]
    struct FakeBackend {
        bound: Mutex<Vec<Option<u16>>>,
    }
    struct FakePunched;

    #[async_trait::async_trait]
    impl TunnelBackend for FakeBackend {
        async fn establish(
            &self,
            _relay_host: &str,
            _secret: &DeviceSecret,
            direct: &DirectOptions,
            on_registered: OnRegistered,
        ) -> anyhow::Result<Box<dyn PunchedTunnel>> {
            self.bound.lock().unwrap().push(direct.bind_port);
            let port = direct.bind_port.unwrap_or(50000);
            let public: SocketAddr = format!("203.0.113.5:{port}").parse().unwrap();
            on_registered(Registered {
                local_port: port,
                public,
                candidates: vec![public],
            });
            Ok(Box::new(FakePunched))
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
                rtt_ms: 1,
                path: "direct",
            });
            std::future::pending().await
        }
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

    #[tokio::test]
    async fn loops_follow_setting_and_revoke() {
        let db = Db::in_memory().unwrap();
        let key = vec![3u8; 32];
        pair(&db, &key, "d1").await;
        let ra = RemoteAccess::new(db.clone(), key, Arc::new(FakeBackend::default()));
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
        let ra = RemoteAccess::new(db.clone(), key.clone(), backend.clone());
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
}
