//! One tunnel at a time (the WebView shows one box). Wraps
//! `peckboard_relay::tunnel::run_device`: binds the box's fixed loopback
//! port, gates it with the per-launch `CookieGate`, and turns `DeviceEvent`s
//! into a `TunnelStatus` the shell UI renders. `pause`/`resume` follow the
//! app lifecycle: no listener and no tunnel while backgrounded.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use peckboard_relay::tunnel::{
    CancellationToken, CookieGate, DeviceEvent, DeviceOptions, ListenAddr, PairingLink, PathKind,
    bind_listener, run_device,
};
use serde::Serialize;
use tauri::async_runtime::{self, JoinHandle};

use crate::nav;

pub const HARD_NAT: &str = "Couldn't reach your PeckBoard from this network right now. Retrying…";
pub const BOX_OFFLINE: &str = "Your PeckBoard isn't reachable right now — it may be offline, or this phone's pairing was revoked.";
/// How long a paused tunnel may take to wind down before we rebind anyway.
const STOP_WAIT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TunnelState {
    Connecting,
    Connected,
    Reconnecting,
    BoxOffline,
    HardNat,
    Error,
    /// App backgrounded; resumes on foreground.
    Paused,
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TunnelStatus {
    pub box_id: String,
    pub state: TunnelState,
    pub message: Option<String>,
    pub rtt_ms: Option<u32>,
    pub retry_in_secs: Option<u64>,
    pub port: u16,
    /// Gate boot URL to navigate the WebView to once `Connected`.
    pub url: String,
    pub ever_connected: bool,
    /// Connected through the relay (no direct path from this network);
    /// the tunnel is still end-to-end encrypted.
    pub relayed: bool,
}

impl TunnelStatus {
    fn new(box_id: &str, port: u16, url: String) -> Self {
        Self {
            box_id: box_id.to_string(),
            state: TunnelState::Connecting,
            message: None,
            rtt_ms: None,
            retry_in_secs: None,
            port,
            url,
            ever_connected: false,
            relayed: false,
        }
    }

    /// Fold one `run_device` event into the status.
    pub fn apply(&mut self, ev: DeviceEvent) {
        use TunnelState::*;
        match ev {
            DeviceEvent::Connecting => {
                self.retry_in_secs = None;
                // Keep a failure on screen while silently retrying it.
                if !matches!(self.state, BoxOffline | HardNat | Error) {
                    self.state = if self.ever_connected {
                        Reconnecting
                    } else {
                        Connecting
                    };
                }
            }
            DeviceEvent::Connected { rtt_ms, path, .. } => {
                self.state = Connected;
                self.rtt_ms = Some(rtt_ms);
                self.relayed = path == PathKind::Relayed;
                self.message = None;
                self.retry_in_secs = None;
                self.ever_connected = true;
            }
            DeviceEvent::PathChanged { path } => self.relayed = path == PathKind::Relayed,
            DeviceEvent::Disconnected { reason } => {
                if reason != "stopped" {
                    self.state = Reconnecting;
                    self.message = Some(format!("Connection lost: {reason}"));
                }
            }
            DeviceEvent::PeerOffline => {
                self.state = BoxOffline;
                self.message = Some(BOX_OFFLINE.into());
            }
            DeviceEvent::PunchFailed { .. } => {
                self.state = HardNat;
                self.message = Some(HARD_NAT.into());
            }
            DeviceEvent::Failed(e) => {
                self.state = Error;
                self.message = Some(e);
            }
            DeviceEvent::Retrying { after } => self.retry_in_secs = Some(after.as_secs()),
        }
    }
}

type Emit = Arc<dyn Fn(&TunnelStatus) + Send + Sync>;

struct Session {
    link: PairingLink,
    generation: u64,
    cancel: Option<CancellationToken>,
    task: Option<JoinHandle<()>>,
    status: TunnelStatus,
}

pub struct TunnelManager {
    /// One gate per app launch, shared by every box (cookies are per host,
    /// not per port). Kept across pause/resume so the WebView's cookie stays
    /// valid.
    gate: CookieGate,
    session: Arc<Mutex<Option<Session>>>,
    /// Serialises start/pause/resume/stop (lifecycle events can race UI).
    ops: tokio::sync::Mutex<()>,
    generation: AtomicU64,
    emit: Emit,
}

impl TunnelManager {
    pub fn new(emit: impl Fn(&TunnelStatus) + Send + Sync + 'static) -> Self {
        Self {
            gate: CookieGate::new(),
            session: Arc::default(),
            ops: tokio::sync::Mutex::new(()),
            generation: AtomicU64::new(0),
            emit: Arc::new(emit),
        }
    }

    pub fn status(&self) -> Option<TunnelStatus> {
        self.session
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.status.clone())
    }

    /// Loopback port the WebView may navigate to (also while paused, so
    /// the box UI page survives backgrounding).
    pub fn active_port(&self) -> Option<u16> {
        self.session.lock().unwrap().as_ref().map(|s| s.status.port)
    }

    /// Connect to `box_id` on its fixed `port`; replaces any other tunnel.
    pub async fn start(
        &self,
        box_id: &str,
        link: PairingLink,
        port: u16,
    ) -> anyhow::Result<TunnelStatus> {
        let _op = self.ops.lock().await;
        {
            let guard = self.session.lock().unwrap();
            if let Some(s) = guard.as_ref()
                && s.status.box_id == box_id
                && s.cancel.is_some()
            {
                return Ok(s.status.clone());
            }
        }
        self.halt().await;
        self.launch(box_id, link, port).await
    }

    /// App went to the background: drop the tunnel and the listener.
    pub async fn pause(&self) {
        let _op = self.ops.lock().await;
        if self.halt().await {
            self.update(|s| {
                s.state = TunnelState::Paused;
                s.retry_in_secs = None;
            });
        }
    }

    /// App is back: reconnect the paused box on the same port (same
    /// origin, so the WebView's page, login and gate cookie stay valid).
    pub async fn resume(&self) -> anyhow::Result<()> {
        let _op = self.ops.lock().await;
        let paused = {
            let guard = self.session.lock().unwrap();
            guard
                .as_ref()
                .filter(|s| s.cancel.is_none() && s.status.state == TunnelState::Paused)
                .map(|s| (s.status.box_id.clone(), s.link.clone(), s.status.port))
        };
        if let Some((id, link, port)) = paused {
            self.launch(&id, link, port).await?;
        }
        Ok(())
    }

    /// Disconnect and forget the session.
    pub async fn stop(&self) {
        let _op = self.ops.lock().await;
        self.halt().await;
        let last = self.session.lock().unwrap().take();
        if let Some(mut s) = last {
            s.status.state = TunnelState::Stopped;
            s.status.retry_in_secs = None;
            (self.emit)(&s.status);
        }
    }

    /// Stop only if `box_id` is the active box (it's being removed).
    pub async fn stop_box(&self, box_id: &str) {
        if self.status().is_some_and(|s| s.box_id == box_id) {
            self.stop().await;
        }
    }

    /// Cancel the running tunnel (if any) and wait for its listener to be
    /// dropped. True if one was running. Caller holds `ops`.
    async fn halt(&self) -> bool {
        let (cancel, task) = {
            let mut guard = self.session.lock().unwrap();
            match guard.as_mut() {
                Some(s) => (s.cancel.take(), s.task.take()),
                None => (None, None),
            }
        };
        let Some(cancel) = cancel else { return false };
        cancel.cancel();
        if let Some(task) = task {
            let _ = tokio::time::timeout(STOP_WAIT, task).await;
        }
        true
    }

    /// Caller holds `ops` and has halted any previous tunnel.
    async fn launch(
        &self,
        box_id: &str,
        link: PairingLink,
        port: u16,
    ) -> anyhow::Result<TunnelStatus> {
        let want = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let listener = bind_listener(ListenAddr::Prefer(want))
            .await
            .context("Couldn't open a local port for the tunnel.")?;
        let bound = listener.local_addr()?.port();
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let cancel = CancellationToken::new();

        let mut status = {
            let guard = self.session.lock().unwrap();
            match guard.as_ref() {
                // Same box again (resume / retry): keep `ever_connected`.
                Some(s) if s.status.box_id == box_id => s.status.clone(),
                _ => TunnelStatus::new(box_id, bound, String::new()),
            }
        };
        status.port = bound;
        status.url = nav::boot_url(bound, &self.gate);
        status.state = if status.ever_connected {
            TunnelState::Reconnecting
        } else {
            TunnelState::Connecting
        };
        status.message = (bound != port).then(|| {
            format!("Port {port} was busy; using {bound} for now (you may need to sign in again).")
        });
        status.retry_in_secs = None;

        let opts = DeviceOptions::new(link.clone()).with_gate(&self.gate);
        let (session, emit) = (self.session.clone(), self.emit.clone());
        let on_event = move |ev: DeviceEvent| {
            log::debug!("tunnel event (gen {generation}): {ev:?}");
            let snapshot = {
                let mut guard = session.lock().unwrap();
                match guard.as_mut() {
                    Some(s) if s.generation == generation => {
                        s.status.apply(ev);
                        s.status.clone()
                    }
                    _ => return,
                }
            };
            emit(&snapshot);
        };
        let token = cancel.clone();
        let task = async_runtime::spawn(async move {
            if let Err(e) = run_device(opts, listener, token, on_event).await {
                log::warn!("tunnel ended: {e:#}");
            }
        });

        *self.session.lock().unwrap() = Some(Session {
            link,
            generation,
            cancel: Some(cancel),
            task: Some(task),
            status: status.clone(),
        });
        (self.emit)(&status);
        Ok(status)
    }

    fn update(&self, f: impl FnOnce(&mut TunnelStatus)) {
        let snapshot = {
            let mut guard = self.session.lock().unwrap();
            let Some(s) = guard.as_mut() else { return };
            f(&mut s.status);
            s.status.clone()
        };
        (self.emit)(&snapshot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use peckboard_relay::keys::PairingSecret;

    #[test]
    fn events_map_to_user_facing_states() {
        let mut s = TunnelStatus::new("b", 41000, String::new());
        s.apply(DeviceEvent::Connecting);
        assert_eq!(s.state, TunnelState::Connecting);
        s.apply(DeviceEvent::PunchFailed { rounds: 4 });
        assert_eq!(s.state, TunnelState::HardNat);
        assert_eq!(s.message.as_deref(), Some(HARD_NAT));
        s.apply(DeviceEvent::Retrying {
            after: Duration::from_secs(4),
        });
        assert_eq!(s.retry_in_secs, Some(4));
        // Retrying keeps the hard-NAT explanation visible.
        s.apply(DeviceEvent::Connecting);
        assert_eq!(s.state, TunnelState::HardNat);
        s.apply(DeviceEvent::Connected {
            peer: "1.2.3.4:5".parse().unwrap(),
            rtt_ms: 30,
            path: PathKind::Relayed,
        });
        assert_eq!(
            (s.state, s.rtt_ms, s.message.clone(), s.relayed),
            (TunnelState::Connected, Some(30), None, true)
        );
        s.apply(DeviceEvent::PathChanged {
            path: PathKind::Direct,
        });
        assert!(!s.relayed);
        s.apply(DeviceEvent::Disconnected {
            reason: "box went away".into(),
        });
        assert_eq!(s.state, TunnelState::Reconnecting);
        s.apply(DeviceEvent::Connecting);
        assert_eq!(s.state, TunnelState::Reconnecting);
        s.apply(DeviceEvent::PeerOffline);
        assert_eq!(s.state, TunnelState::BoxOffline);
    }

    /// pause drops the listener (port refuses connections), resume rebinds
    /// the same port and keeps the gate URL. No relay is contacted: the
    /// link points at an unroutable relay so `run_device` just keeps
    /// retrying in the background.
    #[tokio::test]
    async fn pause_releases_port_and_resume_rebinds_it() {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let mgr = TunnelManager::new(|_| {});
        let link = PairingLink::new(PairingSecret::from_bytes([9; 32]), "127.0.0.1:9");
        let st = mgr.start("b1", link, port).await.unwrap();
        assert_eq!(st.port, port);
        assert!(
            st.url
                .starts_with(&format!("http://127.0.0.1:{port}/__pbm/boot?k="))
        );
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
        );

        mgr.pause().await;
        assert_eq!(mgr.status().unwrap().state, TunnelState::Paused);
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err()
        );

        mgr.resume().await.unwrap();
        let st2 = mgr.status().unwrap();
        assert_eq!((st2.port, st2.url.clone()), (port, st.url));
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
        );

        mgr.stop().await;
        assert!(mgr.status().is_none());
    }
}
