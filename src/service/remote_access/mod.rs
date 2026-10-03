//! Remote access via the relay (relay.peckboard.com).
//!
//! For every paired device ([`crate::db::models::RemoteDevice`]) the box
//! keeps a box-role rendezvous registration alive; when the device shows
//! up, the relay library punches a direct UDP path and the tunnel is
//! served into Peckboard's own router (see [`tunnel`] for why that is not
//! a plain forward to the loopback HTTP port). Failures retry with
//! exponential backoff; per-device status is held in memory for the UI.
//!
//! Globally off by default ([`RemoteAccessSettings::enabled`]); the
//! setting, the relay host, and every device are admin-only
//! (`routes::remote_access`).

pub mod relay;
pub mod secret;
pub mod tunnel;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::Router;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use crate::db::Db;
use crate::routes::settings::{SETTINGS_COLLECTION, SETTINGS_NS};
use secret::DeviceSecret;
use tunnel::{TunnelBackend, TunnelUpdate};

const SETTINGS_KEY: &str = "remote_access";
pub const DEFAULT_RELAY_HOST: &str = "relay.peckboard.com";
const BACKOFF_MIN: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteAccessSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_relay_host")]
    pub relay_host: String,
}

fn default_relay_host() -> String {
    DEFAULT_RELAY_HOST.to_string()
}

impl Default for RemoteAccessSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            relay_host: default_relay_host(),
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

/// Live per-device tunnel state (memory only).
#[derive(Debug, Clone, Serialize)]
pub struct DeviceStatus {
    /// `offline` (remote access disabled) | `waiting` (registered with the
    /// relay, device not connected) | `connected` | `error` (retrying).
    pub state: &'static str,
    pub peer: Option<String>,
    pub rtt_ms: Option<u32>,
    pub error: Option<String>,
}

impl DeviceStatus {
    fn offline() -> Self {
        Self {
            state: "offline",
            peer: None,
            rtt_ms: None,
            error: None,
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
}

pub struct RemoteAccess {
    db: Db,
    vault_key: Vec<u8>,
    backend: Arc<dyn TunnelBackend>,
    /// Peckboard's router, bound once the server has built it.
    app: OnceLock<Router>,
    inner: Mutex<Inner>,
}

impl RemoteAccess {
    pub fn new(db: Db, vault_key: Vec<u8>, backend: Arc<dyn TunnelBackend>) -> Arc<Self> {
        Arc::new(Self {
            db,
            vault_key,
            backend,
            app: OnceLock::new(),
            inner: Mutex::new(Inner::default()),
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
        self.inner
            .lock()
            .unwrap()
            .status
            .get(device_id)
            .cloned()
            .unwrap_or_else(DeviceStatus::offline)
    }

    fn set_status(&self, device_id: &str, s: DeviceStatus) {
        let mut inner = self.inner.lock().unwrap();
        // A revoked/stopped device's loop may race one last update in.
        if inner.tasks.contains_key(device_id) {
            inner.status.insert(device_id.to_string(), s);
        }
    }

    /// Stop one device's loop and drop its tunnel (every open connection
    /// through it dies with the task).
    pub fn stop_device(&self, device_id: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(h) = inner.tasks.remove(device_id) {
            h.abort();
        }
        inner.status.remove(device_id);
    }

    fn stop_all(&self) {
        let mut inner = self.inner.lock().unwrap();
        for (_, h) in inner.tasks.drain() {
            h.abort();
        }
        inner.status.clear();
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
            let relay_host = settings.relay_host.clone();
            let app = app.clone();
            inner.status.insert(d.id.clone(), DeviceStatus::waiting());
            inner.tasks.insert(
                d.id.clone(),
                tokio::spawn(async move { this.device_loop(id, relay_host, app).await }),
            );
        }
    }

    async fn device_loop(self: Arc<Self>, id: String, relay_host: String, app: Router) {
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
            let punched = match self.backend.establish(&relay_host, &secret).await {
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
            let _ = self
                .db
                .touch_remote_device(&id, &chrono::Utc::now().to_rfc3339())
                .await;
            self.set_status(
                &id,
                DeviceStatus {
                    state: "connected",
                    peer: Some(peer.clone()),
                    ..DeviceStatus::offline()
                },
            );
            tracing::info!(device_id = %id, "remote access: device connected");
            let events = {
                let this = Arc::downgrade(&self);
                let id = id.clone();
                Arc::new(move |u: TunnelUpdate| {
                    let Some(this) = this.upgrade() else { return };
                    match u {
                        TunnelUpdate::Connected { rtt_ms } => this.set_status(
                            &id,
                            DeviceStatus {
                                state: "connected",
                                peer: Some(peer.clone()),
                                rtt_ms: Some(rtt_ms),
                                error: None,
                            },
                        ),
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
    use tunnel::{PunchedTunnel, TunnelEvents};

    #[test]
    fn relay_host_validation() {
        assert!(validate_relay_host("relay.peckboard.com").is_ok());
        assert!(validate_relay_host("127.0.0.1:24430").is_ok());
        for bad in ["", "a b", "x:0", "x:99999", "h/path", "user@h"] {
            assert!(validate_relay_host(bad).is_err(), "{bad}");
        }
    }

    /// Connects every pairing at once to a fake peer; `serve` blocks until
    /// the test drops the loop.
    struct FakeBackend;
    struct FakePunched;

    #[async_trait::async_trait]
    impl TunnelBackend for FakeBackend {
        async fn establish(
            &self,
            _relay_host: &str,
            _secret: &DeviceSecret,
        ) -> anyhow::Result<Box<dyn PunchedTunnel>> {
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
            _events: TunnelEvents,
        ) -> anyhow::Result<()> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn loops_follow_setting_and_revoke() {
        let db = Db::in_memory().unwrap();
        let key = vec![3u8; 32];
        let s = DeviceSecret::generate();
        let (ct, nonce) = secret::seal(&key, "d1", &s).unwrap();
        db.insert_remote_device(NewRemoteDevice {
            id: "d1".into(),
            user_id: "u1".into(),
            name: "phone".into(),
            secret_ciphertext: ct,
            secret_nonce: nonce,
            created_at: chrono::Utc::now().to_rfc3339(),
            last_connected_at: None,
        })
        .await
        .unwrap();
        let ra = RemoteAccess::new(db.clone(), key, Arc::new(FakeBackend));
        ra.start(Router::new()).await;
        assert_eq!(ra.status("d1").state, "offline", "default off");

        ra.put_settings(&RemoteAccessSettings {
            enabled: true,
            ..Default::default()
        })
        .await
        .unwrap();
        for _ in 0..100 {
            if ra.status("d1").state == "connected" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let st = ra.status("d1");
        assert_eq!(st.state, "connected");
        assert_eq!(st.peer.as_deref(), Some("198.51.100.7:4444"));
        let row = db.get_remote_device("d1").await.unwrap().unwrap();
        assert!(row.last_connected_at.is_some());

        ra.stop_device("d1");
        assert_eq!(ra.status("d1").state, "offline");
        assert!(ra.inner.lock().unwrap().tasks.is_empty());
    }
}
