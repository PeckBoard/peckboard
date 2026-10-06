//! One tunnel at a time (the WebView shows one box). Wraps
//! `peckboard_relay::tunnel::run_device`: binds the box's fixed loopback
//! port, gates it with a per-box `CookieGate`, and turns `DeviceEvent`s
//! into a `TunnelStatus` the shell UI renders. `pause`/`resume` follow the
//! app lifecycle: no tunnel while backgrounded, but the port stays bound
//! (so another local app can't take over the box page's origin) and the
//! gate key is rotated on resume.
//!
//! Pairing v2: [`TunnelManager::enroll`] runs the enrollment-only round
//! for a fresh v2 link (no box page is ever loaded), then stays on for a
//! background "activate" connect that lets the box retire the link.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use peckboard_relay::tunnel::{
    CancellationToken, CookieGate, DeviceCredential, DeviceEvent, DeviceKick, DeviceOptions,
    ListenAddr, OnEnrolled, PathKind, bind_listener, run_device,
};
use serde::Serialize;
use tauri::async_runtime::{self, JoinHandle};
use tokio::net::TcpListener;

use crate::lock::{LockManager, Unlocked};
use crate::nav;

pub const HARD_NAT: &str = "Couldn't reach your PeckBoard from this network right now. Retrying…";
pub const BOX_OFFLINE: &str = "Your PeckBoard isn't reachable right now — it may be offline, or this phone's pairing was revoked.";
/// `BOX_OFFLINE` while pairing with a link whose advisory expiry passed.
pub const BOX_OFFLINE_OR_EXPIRED: &str =
    "Your PeckBoard isn't reachable right now — it may be offline, or the pairing link expired.";
pub const WRONG_BOX: &str =
    "This link doesn't match the box it reached. Don't continue; create a new link on your box.";
pub const PAIR_TIMEOUT: &str = "Couldn't reach your PeckBoard to finish pairing. Make sure it's online and the link hasn't expired, then try again.";
/// How long a stopped tunnel may take to wind down before its task is
/// aborted.
const STOP_WAIT: Duration = Duration::from_secs(5);
/// The enrollment round must finish within this.
const ENROLL_TIMEOUT: Duration = Duration::from_secs(45);
/// After enrolling, the background activate connect gets this long.
const ACTIVATE_TIMEOUT: Duration = Duration::from_secs(45);

/// What this build tells the box this device is (`device_name_hint`).
pub fn device_name() -> &'static str {
    if cfg!(target_os = "ios") {
        "iPhone"
    } else if cfg!(target_os = "android") {
        "Android phone"
    } else if cfg!(target_os = "macos") {
        "Mac"
    } else if cfg!(windows) {
        "Windows PC"
    } else {
        "PeckBoard app"
    }
}

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

/// A pairing-v2 step the last event completed; the app updates the store
/// on it. Not sent to the shell.
#[derive(Clone, Debug, PartialEq)]
pub enum Milestone {
    /// This device enrolled its key; the credential is already in secure
    /// storage (`on_enrolled`).
    Enrolled {
        box_fingerprint: String,
        legacy_upgrade: bool,
    },
    /// First tunnel on the enrolled credential: the box retired the link.
    Activated,
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
    /// Set by the event that completed a pairing step, cleared by the next.
    #[serde(skip)]
    pub milestone: Option<Milestone>,
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
            milestone: None,
        }
    }

    /// Fold one `run_device` event into the status.
    pub fn apply(&mut self, ev: DeviceEvent) {
        use TunnelState::*;
        self.milestone = None;
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
                // "enrolled": the box closed the enrollment round on
                // purpose; the next round connects for real.
                if reason != "stopped" && reason != "enrolled" {
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
            DeviceEvent::Enrolled {
                box_fingerprint,
                legacy_upgrade,
            } => {
                self.milestone = Some(Milestone::Enrolled {
                    box_fingerprint,
                    legacy_upgrade,
                });
            }
            DeviceEvent::EnrollRefused { reason } => {
                // A legacy upgrade the box declined leaves its tunnel up;
                // a link the box will never accept ends the run.
                if reason.is_final() && !self.ever_connected {
                    self.state = Error;
                    self.message = Some(reason.message().into());
                }
            }
            DeviceEvent::Activated => self.milestone = Some(Milestone::Activated),
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
    cred: DeviceCredential,
    on_enrolled: Option<OnEnrolled>,
    generation: u64,
    /// Bound from `start` until `stop` (or another box) — also while
    /// paused, so no other local app can take the port the box page, its
    /// login and its requests live on.
    listener: Arc<TcpListener>,
    /// This box's gate. Never shared with another box, and replaced on
    /// every run (start / resume) so a key that leaked stops working.
    gate: CookieGate,
    cancel: Option<CancellationToken>,
    /// Network-change kick for the running `run_device`.
    kick: DeviceKick,
    task: Option<JoinHandle<()>>,
    status: TunnelStatus,
}

/// An enrollment / activation round ([`TunnelManager::enroll`]): never a
/// box page, so it has no session; stopped by any real connect.
struct Aux {
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

/// Why an enrollment round didn't enroll.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnrollFailure {
    /// User-facing text.
    pub message: String,
    /// The box will never accept this link (used, expired, not
    /// enrollable): the half-paired record should go.
    pub final_refusal: bool,
}

impl EnrollFailure {
    fn other(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            final_refusal: false,
        }
    }
}

/// Map a failure during the enrollment round to what the user should read
/// (`expires`: the link's advisory expiry, unix seconds).
pub fn enroll_failure(
    ev: &DeviceEvent,
    expires: Option<u64>,
    now_secs: u64,
) -> Option<EnrollFailure> {
    Some(match ev {
        DeviceEvent::EnrollRefused { reason } => EnrollFailure {
            message: reason.message().to_string(),
            final_refusal: reason.is_final(),
        },
        DeviceEvent::PeerOffline => {
            if expires.is_some_and(|e| now_secs > e) {
                EnrollFailure::other(BOX_OFFLINE_OR_EXPIRED)
            } else {
                EnrollFailure::other(BOX_OFFLINE)
            }
        }
        DeviceEvent::PunchFailed { .. } => EnrollFailure::other(
            "Couldn't reach your PeckBoard from this network. Try again from another network.",
        ),
        DeviceEvent::Failed(e) => {
            // The handshake pins the link's box key; a box with another
            // key (or a grant from one) fails exactly there.
            if e.contains("box handshake failed") || e.contains("not from the pinned box key") {
                EnrollFailure::other(WRONG_BOX)
            } else {
                EnrollFailure::other(e.clone())
            }
        }
        _ => return None,
    })
}

pub struct TunnelManager {
    session: Arc<Mutex<Option<Session>>>,
    aux: Mutex<Option<Aux>>,
    /// Serialises start/pause/resume/stop (lifecycle events can race UI).
    ops: tokio::sync::Mutex<()>,
    generation: AtomicU64,
    emit: Emit,
}

impl TunnelManager {
    pub fn new(emit: impl Fn(&TunnelStatus) + Send + Sync + 'static) -> Self {
        Self {
            session: Arc::default(),
            aux: Mutex::new(None),
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

    /// The OS reports a new default network: drop the tunnel on the old
    /// path and reconnect at once instead of waiting for the ping timeout.
    /// Port and gate key stay, so the box page just stalls briefly. No-op
    /// when no tunnel is running (none, paused) and within the kick
    /// debounce. True if a reconnect was triggered.
    pub fn network_changed(&self) -> bool {
        let guard = self.session.lock().unwrap();
        match guard.as_ref() {
            Some(s) if s.cancel.is_some() => s.kick.network_changed(),
            _ => false,
        }
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

    /// Connect to `box_id` on its fixed `port`; replaces any other tunnel
    /// (and any enrollment round). `on_enrolled` stores the credential
    /// should this run enroll (a v2 link not yet enrolled, or a legacy
    /// pairing the box upgrades). Needs [`Unlocked`]: no tunnel while the
    /// app lock is engaged.
    pub async fn start(
        &self,
        _unlocked: &Unlocked,
        box_id: &str,
        cred: DeviceCredential,
        port: u16,
        on_enrolled: Option<OnEnrolled>,
    ) -> anyhow::Result<TunnelStatus> {
        let _op = self.ops.lock().await;
        self.stop_aux().await;
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
        self.launch(box_id, cred, on_enrolled, listener, port, false)
    }

    /// App went to the background: drop the tunnel, keep the listener
    /// bound (unaccepted) so the box's port can't be taken meanwhile.
    pub async fn pause(&self) {
        let _op = self.ops.lock().await;
        self.stop_aux().await;
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
    /// Needs [`Unlocked`]: a locked app stays paused.
    pub async fn resume(&self, _unlocked: &Unlocked) -> anyhow::Result<Option<TunnelStatus>> {
        let _op = self.ops.lock().await;
        let paused = {
            let guard = self.session.lock().unwrap();
            guard
                .as_ref()
                .filter(|s| s.cancel.is_none() && s.status.state == TunnelState::Paused)
                .map(|s| {
                    (
                        s.status.box_id.clone(),
                        s.cred.clone(),
                        s.on_enrolled.clone(),
                        s.listener.clone(),
                        s.status.port,
                    )
                })
        };
        match paused {
            Some((id, cred, on_enrolled, listener, port)) => Ok(Some(self.launch(
                &id,
                cred,
                on_enrolled,
                listener,
                port,
                true,
            )?)),
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
            s.status.milestone = None;
            (self.emit)(&s.status);
        }
    }

    /// The app lock engaged: stop the tunnel and any pairing round.
    pub async fn stop_all(&self) {
        {
            let _op = self.ops.lock().await;
            self.stop_aux().await;
        }
        self.stop().await;
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

    /// End any enrollment / activation round.
    async fn stop_aux(&self) {
        let aux = self.aux.lock().unwrap().take();
        if let Some(Aux { cancel, mut task }) = aux {
            cancel.cancel();
            if tokio::time::timeout(STOP_WAIT, &mut task).await.is_err() {
                task.abort();
            }
        }
    }

    /// Run the tunnel for `box_id` on `listener` with a fresh gate. Caller
    /// holds `ops` and has halted any previous run. `port`: the box's
    /// preferred port (to explain a fallback).
    fn launch(
        &self,
        box_id: &str,
        cred: DeviceCredential,
        on_enrolled: Option<OnEnrolled>,
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
        status.milestone = None;
        status.state = if status.ever_connected {
            TunnelState::Reconnecting
        } else {
            TunnelState::Connecting
        };
        status.message = (bound != port).then(|| {
            format!("Port {port} was busy; using {bound} for now (you may need to sign in again).")
        });
        status.retry_in_secs = None;

        let mut opts = DeviceOptions::new(cred.clone()).with_gate(&gate);
        opts.device_name = device_name().to_string();
        // Persist first (the caller's hook), then make the next resume use
        // the enrolled credential rather than the link.
        opts.on_enrolled = on_enrolled.clone().map(|persist| {
            let session = self.session.clone();
            let hook: OnEnrolled = Arc::new(move |c| {
                persist(c)?;
                if let Some(s) = session.lock().unwrap().as_mut()
                    && s.generation == generation
                {
                    s.cred = DeviceCredential::Enrolled(c.clone());
                }
                Ok(())
            });
            hook
        });
        let kick = opts.kick.clone();
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
                        s.status.milestone = None;
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
            cred,
            on_enrolled,
            generation,
            listener,
            gate,
            cancel: Some(cancel),
            kick,
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

    /// Pair with a v2 link: one `run_device` on an ephemeral, gated port
    /// nothing ever navigates to. Returns once the box granted this device
    /// its key (`on_enrolled` has stored the credential) with the box
    /// fingerprint, or with why not — within [`ENROLL_TIMEOUT`]. After a
    /// grant the run keeps going in the background for the activate
    /// connect (the box retires the link on the first tunnel with the new
    /// credential; `on_activated` then runs), capped at
    /// [`ACTIVATE_TIMEOUT`]; any real connect ends it early. No box page
    /// is loaded at any point. Refused while the app is locked; a round
    /// that started before the lock engaged is ended by [`Self::stop_all`].
    pub async fn enroll(
        &self,
        lock: &LockManager,
        cred: DeviceCredential,
        expires: Option<u64>,
        on_enrolled: OnEnrolled,
        on_activated: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<String, EnrollFailure> {
        {
            let _op = self.ops.lock().await;
            self.stop_aux().await;
        }
        let listener = bind_listener(ListenAddr::Ephemeral)
            .await
            .map_err(|e| EnrollFailure::other(format!("Couldn't open a local port: {e}")))?;
        // Held until the round is registered in `aux`, so a lock engaging
        // meanwhile waits for it and its `stop_all` then ends it.
        let unlocked = lock.unlocked().await.map_err(EnrollFailure::other)?;
        // A gate whose key nobody has: the port admits nothing.
        let gate = CookieGate::new();
        let mut opts = DeviceOptions::new(cred).with_gate(&gate);
        opts.device_name = device_name().to_string();
        opts.on_enrolled = Some(on_enrolled);
        opts.give_up_on_punch_failure = true;
        opts.max_backoff = Duration::from_secs(5);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeviceEvent>();
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let task = async_runtime::spawn(async move {
            let r = run_device(opts, listener, token, move |ev| {
                log::debug!("enroll event: {ev:?}");
                let _ = tx.send(ev);
            })
            .await;
            if let Err(e) = r {
                log::info!("enrollment round ended: {e:#}");
            }
        });
        let aux = Aux {
            cancel: cancel.clone(),
            task,
        };
        *self.aux.lock().unwrap() = Some(aux);
        drop(unlocked);

        let now = || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        };
        let deadline = tokio::time::Instant::now() + ENROLL_TIMEOUT;
        let outcome = loop {
            let ev = match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(ev)) => ev,
                Ok(None) => break Err(EnrollFailure::other(PAIR_TIMEOUT)),
                Err(_) => break Err(EnrollFailure::other(PAIR_TIMEOUT)),
            };
            if let DeviceEvent::Enrolled {
                box_fingerprint, ..
            } = &ev
            {
                break Ok(box_fingerprint.clone());
            }
            if let Some(f) = enroll_failure(&ev, expires, now()) {
                break Err(f);
            }
        };
        match outcome {
            Ok(fp) => {
                // Activate in the background; the box retires S on the
                // first tunnel with the new credential.
                let aux_cancel = cancel.clone();
                async_runtime::spawn(async move {
                    let deadline = tokio::time::Instant::now() + ACTIVATE_TIMEOUT;
                    loop {
                        match tokio::time::timeout_at(deadline, rx.recv()).await {
                            Ok(Some(DeviceEvent::Activated)) => {
                                log::info!("pairing activated");
                                on_activated();
                                break;
                            }
                            Ok(Some(_)) => continue,
                            Ok(None) => break,
                            Err(_) => {
                                log::info!("activation deferred to the first open");
                                break;
                            }
                        }
                    }
                    aux_cancel.cancel();
                });
                Ok(fp)
            }
            Err(f) => {
                cancel.cancel();
                Err(f)
            }
        }
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
    use peckboard_relay::tunnel::{PairingLink, RefuseReason};

    fn legacy_link() -> DeviceCredential {
        DeviceCredential::Legacy {
            link: PairingLink::new(PairingSecret::from_bytes([9; 32]), "127.0.0.1:9"),
            upgrade_key: None,
        }
    }

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

    /// Pairing v2 events: enrolling and activating are milestones (one
    /// event long), the box's "enrolled" close is not a lost connection,
    /// and a final refusal of a fresh link is an error.
    #[test]
    fn pairing_events_become_milestones_not_errors() {
        let mut s = TunnelStatus::new("b", 41000, String::new());
        s.apply(DeviceEvent::Connecting);
        s.apply(DeviceEvent::Enrolled {
            box_fingerprint: "AAAA-BBBB-CCCC-DDDD".into(),
            legacy_upgrade: false,
        });
        assert_eq!(
            s.milestone,
            Some(Milestone::Enrolled {
                box_fingerprint: "AAAA-BBBB-CCCC-DDDD".into(),
                legacy_upgrade: false
            })
        );
        assert_eq!(s.state, TunnelState::Connecting);
        s.apply(DeviceEvent::Disconnected {
            reason: "enrolled".into(),
        });
        assert_eq!(
            (s.state, s.milestone.clone(), s.message.clone()),
            (TunnelState::Connecting, None, None)
        );
        s.apply(DeviceEvent::Retrying {
            after: Duration::ZERO,
        });
        s.apply(DeviceEvent::Connecting);
        s.apply(DeviceEvent::Connected {
            peer: "1.2.3.4:5".parse().unwrap(),
            rtt_ms: 30,
            path: PathKind::Direct,
        });
        s.apply(DeviceEvent::Activated);
        assert_eq!(s.milestone, Some(Milestone::Activated));
        assert_eq!(s.state, TunnelState::Connected);
        // A legacy upgrade the box declined: the tunnel is still up.
        s.apply(DeviceEvent::EnrollRefused {
            reason: RefuseReason::NotEnrollable,
        });
        assert_eq!(s.state, TunnelState::Connected);

        let mut fresh = TunnelStatus::new("c", 41001, String::new());
        fresh.apply(DeviceEvent::EnrollRefused {
            reason: RefuseReason::AlreadyUsed,
        });
        assert_eq!(fresh.state, TunnelState::Error);
        assert_eq!(
            fresh.message.as_deref(),
            Some(RefuseReason::AlreadyUsed.message())
        );
    }

    /// §5.6: what the user reads when the enrollment round fails.
    #[test]
    fn enrollment_failures_read_as_spec_messages() {
        let f =
            |ev: DeviceEvent, exp: Option<u64>, now: u64| enroll_failure(&ev, exp, now).unwrap();
        let used = f(
            DeviceEvent::EnrollRefused {
                reason: RefuseReason::AlreadyUsed,
            },
            None,
            0,
        );
        assert!(used.final_refusal);
        assert!(used.message.contains("already used"), "{}", used.message);
        let expired = f(
            DeviceEvent::EnrollRefused {
                reason: RefuseReason::Expired,
            },
            None,
            0,
        );
        assert!(expired.final_refusal && expired.message.contains("expired"));
        let limited = f(
            DeviceEvent::EnrollRefused {
                reason: RefuseReason::RateLimited,
            },
            None,
            0,
        );
        assert!(!limited.final_refusal);
        // Box offline: plain, or "…or the link expired" past the expiry.
        assert_eq!(
            f(DeviceEvent::PeerOffline, Some(100), 50).message,
            BOX_OFFLINE
        );
        assert_eq!(
            f(DeviceEvent::PeerOffline, Some(100), 150).message,
            BOX_OFFLINE_OR_EXPIRED
        );
        assert_eq!(f(DeviceEvent::PeerOffline, None, 150).message, BOX_OFFLINE);
        // Wrong box: the pinned-key handshake fails.
        let wrong = f(
            DeviceEvent::Failed("box handshake failed: invalid peer certificate".into()),
            None,
            0,
        );
        assert_eq!(wrong.message, WRONG_BOX);
        assert!(!wrong.final_refusal);
        // Old box: passed through.
        let old = f(
            DeviceEvent::Failed(
                "this box doesn't support this pairing link; update Peckboard on the box".into(),
            ),
            None,
            0,
        );
        assert!(old.message.contains("update Peckboard on the box"));
        // Progress events are not failures.
        for ev in [
            DeviceEvent::Connecting,
            DeviceEvent::Retrying {
                after: Duration::ZERO,
            },
            DeviceEvent::Disconnected {
                reason: "enrolled".into(),
            },
        ] {
            assert!(enroll_failure(&ev, None, 0).is_none());
        }
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
        let u = Unlocked::for_tests();
        let st = mgr
            .start(&u, "b1", legacy_link(), port, None)
            .await
            .unwrap();
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

        let st2 = mgr.resume(&u).await.unwrap().unwrap();
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
        let u = Unlocked::for_tests();
        let a = mgr
            .start(&u, "a", legacy_link(), free_port(), None)
            .await
            .unwrap();
        let b = mgr
            .start(&u, "b", legacy_link(), free_port(), None)
            .await
            .unwrap();
        assert_ne!(key(&a.url), key(&b.url));
        assert!(port_is_free(a.port), "switching box kept the old port");
        mgr.stop().await;
    }

    /// A network change kicks only a running tunnel, and never moves its
    /// port or rotates its gate key (the box page stays valid).
    #[tokio::test]
    async fn network_change_kicks_only_a_running_tunnel() {
        let mgr = TunnelManager::new(|_| {});
        assert!(!mgr.network_changed(), "no tunnel");
        let st = mgr
            .start(
                &Unlocked::for_tests(),
                "b",
                legacy_link(),
                free_port(),
                None,
            )
            .await
            .unwrap();
        assert!(mgr.network_changed());
        assert!(!mgr.network_changed(), "not debounced");
        let now = mgr.status().unwrap();
        assert_eq!((now.port, key(&now.url)), (st.port, key(&st.url)));
        mgr.pause().await;
        // Past the debounce, so only "paused" can refuse the kick.
        tokio::time::sleep(peckboard_relay::tunnel::KICK_DEBOUNCE).await;
        assert!(!mgr.network_changed(), "kicked a paused tunnel");
        mgr.stop().await;
    }
}
