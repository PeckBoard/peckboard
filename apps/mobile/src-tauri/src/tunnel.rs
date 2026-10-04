//! One tunnel at a time (the WebView shows one box). Wraps
//! `peckboard_relay::tunnel::run_device`: binds the box's fixed loopback
//! port, gates it with a per-box `CookieGate`, and turns `DeviceEvent`s
//! into a `TunnelStatus` the shell UI renders. `pause`/`resume` follow the
//! app lifecycle: no tunnel while backgrounded, but the port stays bound
//! (so another local app can't take over the box page's origin) and the
//! gate key is rotated on resume.

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
use tokio::net::TcpListener;

use crate::nav;

pub const HARD_NAT: &str = "Couldn't reach your PeckBoard from this network right now. Retrying…";
pub const BOX_OFFLINE: &str = "Your PeckBoard isn't reachable right now — it may be offline, or this phone's pairing was revoked.";
/// How long a stopped tunnel may take to wind down before its task is
/// aborted.
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
    /// The gate key changed since the WebView last booted (resume): once
    /// `Connected`, a box page still showing must re-boot through `url`.
    /// Set on the first `Connected` status only; not sent to the shell.
    #[serde(skip)]
    pub rekeyed: bool,
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
            rekeyed: false,
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

    /// The Boxes button's "Relayed" badge: up and relayed. Cleared while
    /// reconnecting (the next path is unknown) or not running.
    pub fn relay_badge(&self) -> bool {
        self.state == TunnelState::Connected && self.relayed
    }
}

type Emit = Arc<dyn Fn(&TunnelStatus) + Send + Sync>;

struct Session {
    link: PairingLink,
    generation: u64,
    /// Bound from `start` until `stop` (or another box) — also while
    /// paused, so no other local app can take the port the box page, its
    /// login and its requests live on.
    listener: Arc<TcpListener>,
    /// This box's gate. Never shared with another box, and replaced on
    /// every run (start / resume) so a key that leaked stops working.
    gate: CookieGate,
    cancel: Option<CancellationToken>,
    task: Option<JoinHandle<()>>,
    status: TunnelStatus,
}

pub struct TunnelManager {
    session: Arc<Mutex<Option<Session>>>,
    /// Serialises start/pause/resume/stop (lifecycle events can race UI).
    ops: tokio::sync::Mutex<()>,
    generation: AtomicU64,
    emit: Emit,
}

impl TunnelManager {
    pub fn new(emit: impl Fn(&TunnelStatus) + Send + Sync + 'static) -> Self {
        Self {
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

    /// The active box's boot URL (current gate key) landing on `next`.
    pub fn boot_url_to(&self, next: &str) -> Option<String> {
        let guard = self.session.lock().unwrap();
        let s = guard.as_ref()?;
        Some(format!(
            "{}{}",
            nav::origin(s.status.port),
            s.gate.boot_path_to(next)
        ))
    }

    /// Connect to `box_id` on its fixed `port`; replaces any other tunnel.
    pub async fn start(
        &self,
        box_id: &str,
        link: PairingLink,
        port: u16,
    ) -> anyhow::Result<TunnelStatus> {
        let _op = self.ops.lock().await;
        let held = {
            let guard = self.session.lock().unwrap();
            match guard.as_ref() {
                Some(s) if s.status.box_id == box_id && s.cancel.is_some() => {
                    return Ok(s.status.clone());
                }
                // Same box, not running (paused): keep its port.
                Some(s) if s.status.box_id == box_id => Some(s.listener.clone()),
                _ => None,
            }
        };
        let listener = match held {
            Some(l) => l,
            None => {
                self.halt().await;
                // Another box's listener goes with its session.
                self.session.lock().unwrap().take();
                let want = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
                let l = bind_listener(ListenAddr::Prefer(want))
                    .await
                    .context("Couldn't open a local port for the tunnel.")?;
                Arc::new(l)
            }
        };
        self.launch(box_id, link, listener, port, false)
    }

    /// App went to the background: drop the tunnel, keep the listener
    /// bound (unaccepted) so the box's port can't be taken meanwhile.
    pub async fn pause(&self) {
        let _op = self.ops.lock().await;
        if self.halt().await {
            self.update(|s| {
                s.state = TunnelState::Paused;
                s.retry_in_secs = None;
            });
        }
    }

    /// App is back: reconnect the paused box on the port it kept (same
    /// origin, so the WebView's page and login stay valid) with a fresh
    /// gate key. The returned status has `rekeyed` set; once `Connected`
    /// the WebView must re-boot through the new key ([`Self::boot_url_to`]).
    pub async fn resume(&self) -> anyhow::Result<Option<TunnelStatus>> {
        let _op = self.ops.lock().await;
        let paused = {
            let guard = self.session.lock().unwrap();
            guard
                .as_ref()
                .filter(|s| s.cancel.is_none() && s.status.state == TunnelState::Paused)
                .map(|s| {
                    (
                        s.status.box_id.clone(),
                        s.link.clone(),
                        s.listener.clone(),
                        s.status.port,
                    )
                })
        };
        match paused {
            Some((id, link, listener, port)) => {
                Ok(Some(self.launch(&id, link, listener, port, true)?))
            }
            None => Ok(None),
        }
    }

    /// Disconnect, release the port and forget the session.
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

    /// Cancel the running tunnel (if any) and wait for it to end, so no
    /// stale gate keeps accepting on the held listener. True if one was
    /// running. Caller holds `ops`.
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
        if let Some(mut task) = task
            && tokio::time::timeout(STOP_WAIT, &mut task).await.is_err()
        {
            task.abort();
        }
        true
    }

    /// Run the tunnel for `box_id` on `listener` with a fresh gate. Caller
    /// holds `ops` and has halted any previous run. `port`: the box's
    /// preferred port (to explain a fallback).
    fn launch(
        &self,
        box_id: &str,
        link: PairingLink,
        listener: Arc<TcpListener>,
        port: u16,
        rekeyed: bool,
    ) -> anyhow::Result<TunnelStatus> {
        let bound = listener.local_addr()?.port();
        let gate = CookieGate::new();
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
        status.url = nav::boot_url(bound, &gate);
        status.rekeyed = rekeyed;
        status.state = if status.ever_connected {
            TunnelState::Reconnecting
        } else {
            TunnelState::Connecting
        };
        status.message = (bound != port).then(|| {
            format!("Port {port} was busy; using {bound} for now (you may need to sign in again).")
        });
        status.retry_in_secs = None;

        let opts = DeviceOptions::new(link.clone()).with_gate(&gate);
        let (session, emit) = (self.session.clone(), self.emit.clone());
        let on_event = move |ev: DeviceEvent| {
            log::debug!("tunnel event (gen {generation}): {ev:?}");
            let snapshot = {
                let mut guard = session.lock().unwrap();
                match guard.as_mut() {
                    Some(s) if s.generation == generation => {
                        s.status.apply(ev);
                        let snapshot = s.status.clone();
                        // `rekeyed` reaches the first `Connected` only.
                        if s.status.state == TunnelState::Connected {
                            s.status.rekeyed = false;
                        }
                        snapshot
                    }
                    _ => return,
                }
            };
            emit(&snapshot);
        };
        #[cfg(debug_assertions)]
        debug_path_flip(on_event.clone(), cancel.clone());
        let token = cancel.clone();
        let run_listener = listener.clone();
        let task = async_runtime::spawn(async move {
            if let Err(e) = run_device(opts, run_listener, token, on_event).await {
                log::warn!("tunnel ended: {e:#}");
            }
        });

        *self.session.lock().unwrap() = Some(Session {
            link,
            generation,
            listener,
            gate,
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

/// Debug builds: `PBM_DEBUG_PATH_FLIP_SECS=<n>` reports a path change
/// (relayed, direct, …) every `n` s until `cancel`, to check the "Relayed"
/// badge where punching always works (simulator on a LAN).
#[cfg(debug_assertions)]
fn debug_path_flip(on_event: impl Fn(DeviceEvent) + Send + 'static, cancel: CancellationToken) {
    let Some(secs) = std::env::var("PBM_DEBUG_PATH_FLIP_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&s| s > 0)
    else {
        return;
    };
    async_runtime::spawn(async move {
        for path in [PathKind::Relayed, PathKind::Direct].into_iter().cycle() {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
                _ = cancel.cancelled() => return,
            }
            on_event(DeviceEvent::PathChanged { path });
        }
    });
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

    #[test]
    fn relay_badge_follows_the_live_path() {
        let shell = [url::Url::parse("tauri://localhost").unwrap()];
        let page = url::Url::parse("http://127.0.0.1:41000/sessions").unwrap();
        let badge = |s: &TunnelStatus| {
            nav::relay_badge_script(&page, &shell, Some(s.port), s.relay_badge()).unwrap()
        };
        let (on, off) = (r#"("http://127.0.0.1:41000", true);"#, ", false);");
        let mut s = TunnelStatus::new("b", 41000, String::new());
        assert!(badge(&s).ends_with(off));
        s.apply(DeviceEvent::Connected {
            peer: "1.2.3.4:5".parse().unwrap(),
            rtt_ms: 30,
            path: PathKind::Relayed,
        });
        assert!(badge(&s).ends_with(on));
        s.apply(DeviceEvent::PathChanged {
            path: PathKind::Direct,
        });
        assert!(badge(&s).ends_with(off));
        s.apply(DeviceEvent::PathChanged {
            path: PathKind::Relayed,
        });
        assert!(badge(&s).ends_with(on));
        // No path while reconnecting: no badge until it says which.
        s.apply(DeviceEvent::Disconnected {
            reason: "box went away".into(),
        });
        assert!(badge(&s).ends_with(off));
        let js = badge(&s);
        assert!(js.contains(r#"setAttribute("data-relayed""#));
        assert!(!js.contains("__TAURI") && !js.contains("invoke"), "no IPC");
        // Never onto the shell, the boot page or another port.
        for other in [
            "tauri://localhost/",
            "http://127.0.0.1:41000/__pbm/boot?k=x",
            "http://127.0.0.1:41001/",
        ] {
            let u = url::Url::parse(other).unwrap();
            assert_eq!(nav::relay_badge_script(&u, &shell, Some(41000), true), None);
        }
    }

    fn free_port() -> u16 {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    }

    fn port_is_free(port: u16) -> bool {
        std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
    }

    fn key(url: &str) -> String {
        url.split("k=").nth(1).unwrap().to_string()
    }

    /// SECURITY (port squatting): pause keeps the port bound — another
    /// local app can't take it while the box page is still alive — and
    /// resume reuses it with a fresh gate key, so a cookie that leaked
    /// stops working. No relay is contacted: the link points at an
    /// unroutable relay so `run_device` just keeps retrying.
    #[tokio::test]
    async fn pause_keeps_port_and_resume_rotates_gate_key() {
        let port = free_port();
        let mgr = TunnelManager::new(|_| {});
        let link = PairingLink::new(PairingSecret::from_bytes([9; 32]), "127.0.0.1:9");
        let st = mgr.start("b1", link, port).await.unwrap();
        assert_eq!(st.port, port);
        assert!(
            st.url
                .starts_with(&format!("http://127.0.0.1:{port}/__pbm/boot?k="))
        );
        assert!(!st.rekeyed);
        assert!(!port_is_free(port));

        mgr.pause().await;
        assert_eq!(mgr.status().unwrap().state, TunnelState::Paused);
        assert!(!port_is_free(port), "paused tunnel released its port");

        let st2 = mgr.resume().await.unwrap().unwrap();
        assert_eq!(st2.port, port);
        assert!(st2.rekeyed);
        assert_ne!(key(&st2.url), key(&st.url), "gate key not rotated");
        let reboot = mgr.boot_url_to("/sessions/x?a=1").unwrap();
        assert!(reboot.starts_with(&st2.url), "{reboot}");
        assert!(reboot.ends_with("&next=/sessions/x%3Fa%3D1"), "{reboot}");
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
        );

        mgr.stop().await;
        assert!(mgr.status().is_none());
        assert!(port_is_free(port), "stop kept the port");
    }

    /// Each box gets its own gate key, so one box never learns another's.
    #[tokio::test]
    async fn each_box_gets_its_own_gate_key() {
        let mgr = TunnelManager::new(|_| {});
        let link = PairingLink::new(PairingSecret::from_bytes([9; 32]), "127.0.0.1:9");
        let a = mgr.start("a", link.clone(), free_port()).await.unwrap();
        let b = mgr.start("b", link, free_port()).await.unwrap();
        assert_ne!(key(&a.url), key(&b.url));
        assert!(port_is_free(a.port), "switching box kept the old port");
        mgr.stop().await;
    }
}
