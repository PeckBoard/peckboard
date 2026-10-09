//! Interactive SSH terminals — real PTY shells on plugin-registered hosts,
//! shown as full-size tabs in the main app (`web/src/components/terminal/`).
//!
//! ## Data Flow
//!
//! A terminal is a DB row ([`crate::db::models::Terminal`]): owner, host
//! REFERENCE (`plugin_id` + `host_id`), display name, remote tmux session
//! name. No credential is ever stored. A viewer attaches over
//! `/ws/terminal/{id}` ([`crate::ws::terminal`]); the first attach after
//! boot (or after the shell ended) starts a **driver** task here, which:
//!
//! 1. resolves the host through the owning plugin
//!    ([`resolver::HostResolver`] → the `terminal.host.resolve` hook), in
//!    memory only — credentials never reach the browser, DB, or logs;
//! 2. opens a dedicated SSH connection (TCP_NODELAY, keepalives, no
//!    inactivity timeout — [`ssh::connect_interactive`]);
//! 3. probes for tmux. With tmux the shell runs inside
//!    `tmux -L peckboard … attach-session -t peck-<id>`, configured to behave
//!    like a plain shell (no status bar, no prefix key, `escape-time 0`,
//!    mouse passthrough — see [`tmux_attach_script`]), so it survives Peckboard restarts,
//!    browser closes and network drops. Without tmux it is a plain login
//!    shell (`persistent = false`; the UI says so);
//! 4. requests an `xterm-256color` PTY and pumps raw bytes both ways.
//!
//! Output fans out to every attached viewer over a broadcast channel and is
//! kept in a bounded scrollback ring, so a viewer that attaches later (page
//! reload, pop-out window, second device) replays it instantly.
//!
//! ## Reconnect
//!
//! When the connection drops while viewers are attached the driver
//! reconnects with backoff and reattaches the same tmux session, then forces
//! a full redraw. With no viewers it parks as `idle` and the next attach
//! reconnects. When the shell itself exits (the user typed `exit`) the
//! terminal is `ended`; a viewer may restart it.

pub mod resolver;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use russh::{ChannelMsg, Disconnect, client};
use serde::Serialize;
use tokio::sync::{Notify, broadcast, mpsc, watch};

use crate::db::Db;
use crate::db::models::Terminal as TerminalRow;
use crate::plugin::ssh::{self, Live};
pub use resolver::{HostEntry, HostResolver, PluginHostResolver, ResolvedHost};

/// Raw PTY output kept per terminal and replayed into a fresh xterm.
pub const SCROLLBACK_CAP: usize = 1024 * 1024;
/// Broadcast depth per terminal. A viewer that falls this far behind gets a
/// `resync` nudge and re-attaches from the scrollback.
const OUTPUT_CHANNEL_CAP: usize = 4096;
/// Production tmux socket (`tmux -L <name>`): isolates our sessions from the
/// user's own tmux server and its config.
pub const TMUX_SOCKET: &str = "peckboard";
/// Reconnect backoff (seconds), last value repeats.
const BACKOFF_SECS: [u64; 6] = [1, 2, 4, 8, 15, 30];
/// Budget for the short helper commands (tmux probe / has-session / kill).
const EXEC_TIMEOUT: Duration = Duration::from_secs(10);
/// `last_active_at` is bumped at most this often while the user types.
const TOUCH_EVERY: Duration = Duration::from_secs(30);
/// PTY geometry bounds.
pub const MAX_COLS: u16 = 1000;
pub const MAX_ROWS: u16 = 500;

// ───────────────────────────── scrollback ring ──────────────────────────────

/// Bounded ring of the newest output bytes.
pub struct Scrollback {
    buf: VecDeque<u8>,
    cap: usize,
}

impl Scrollback {
    pub fn new(cap: usize) -> Self {
        Scrollback {
            buf: VecDeque::new(),
            cap,
        }
    }

    /// Append, evicting the oldest bytes past the cap.
    pub fn push(&mut self, data: &[u8]) {
        if data.len() >= self.cap {
            self.buf.clear();
            self.buf.extend(&data[data.len() - self.cap..]);
            return;
        }
        let overflow = (self.buf.len() + data.len()).saturating_sub(self.cap);
        if overflow > 0 {
            self.buf.drain(..overflow);
        }
        self.buf.extend(data);
    }

    pub fn snapshot(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

// ─────────────────────────────── status ─────────────────────────────────────

/// Where a terminal's shell is. `Idle` = no connection right now (after a
/// restart, or no viewer since the connection dropped); the next attach
/// connects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Idle,
    Connecting,
    Live,
    Reconnecting,
    Ended,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Status {
    pub phase: Phase,
    /// Whether the shell runs inside tmux. `None` until the first connect.
    pub persistent: Option<bool>,
    /// Human-readable detail for `error` / `reconnecting` / `ended`.
    pub message: Option<String>,
}

/// Commands viewers send to the live channel.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TermCmd {
    Input(Bytes),
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Force a full-screen redraw (tmux reattach / fresh viewer).
    Redraw,
    /// End the shell for good (kill the tmux session) and disconnect.
    Close,
    /// Test seam: drop the SSH connection as a network failure would.
    #[cfg(test)]
    DropConnection,
}

struct OutState {
    scrollback: Scrollback,
    tx: broadcast::Sender<Bytes>,
}

/// One terminal's live state. Lives in [`TerminalManager`] for as long as
/// the terminal is open (not closed), across any number of connections.
pub struct TermSession {
    pub id: String,
    pub user_id: String,
    plugin_id: String,
    host_id: String,
    tmux_session: String,
    out: Mutex<OutState>,
    status: watch::Sender<Status>,
    cmd_tx: Mutex<Option<mpsc::UnboundedSender<TermCmd>>>,
    size: Mutex<(u16, u16)>,
    viewers: AtomicUsize,
    driver_running: AtomicBool,
    closing: AtomicBool,
    /// Wakes a driver sleeping in reconnect backoff (new viewer / close).
    wake: Notify,
    last_touch: Mutex<Option<Instant>>,
}

impl TermSession {
    fn new(row: &TerminalRow) -> Arc<Self> {
        let (tx, _) = broadcast::channel(OUTPUT_CHANNEL_CAP);
        let (status, _) = watch::channel(Status {
            phase: Phase::Idle,
            persistent: None,
            message: None,
        });
        Arc::new(TermSession {
            id: row.id.clone(),
            user_id: row.user_id.clone(),
            plugin_id: row.plugin_id.clone(),
            host_id: row.host_id.clone(),
            tmux_session: row.tmux_session.clone(),
            out: Mutex::new(OutState {
                scrollback: Scrollback::new(SCROLLBACK_CAP),
                tx,
            }),
            status,
            cmd_tx: Mutex::new(None),
            size: Mutex::new((80, 24)),
            viewers: AtomicUsize::new(0),
            driver_running: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            wake: Notify::new(),
            last_touch: Mutex::new(None),
        })
    }

    /// Everything so far plus a receiver for what follows, gap-free.
    pub fn attach_output(&self) -> (Vec<u8>, broadcast::Receiver<Bytes>) {
        let out = self.out.lock().expect("terminal output poisoned");
        (out.scrollback.snapshot(), out.tx.subscribe())
    }

    fn push_output(&self, data: &[u8]) {
        let mut out = self.out.lock().expect("terminal output poisoned");
        out.scrollback.push(data);
        let _ = out.tx.send(Bytes::copy_from_slice(data));
    }

    fn send(&self, cmd: TermCmd) -> bool {
        self.cmd_tx
            .lock()
            .expect("terminal cmd poisoned")
            .as_ref()
            .is_some_and(|tx| tx.send(cmd).is_ok())
    }

    /// Keystrokes for the shell. Dropped (returns `false`) while there is no
    /// live connection — typing into a reconnecting shell must not replay
    /// later as a burst.
    pub fn input(&self, data: Bytes) -> bool {
        self.send(TermCmd::Input(data))
    }

    /// New PTY geometry (last viewer to resize wins).
    pub fn resize(&self, cols: u16, rows: u16) {
        let cols = cols.clamp(2, MAX_COLS);
        let rows = rows.clamp(1, MAX_ROWS);
        *self.size.lock().expect("terminal size poisoned") = (cols, rows);
        self.send(TermCmd::Resize { cols, rows });
    }

    pub fn size(&self) -> (u16, u16) {
        *self.size.lock().expect("terminal size poisoned")
    }

    pub fn status(&self) -> Status {
        self.status.borrow().clone()
    }

    pub fn status_rx(&self) -> watch::Receiver<Status> {
        self.status.subscribe()
    }

    fn set_phase(&self, phase: Phase, message: Option<String>) {
        self.status.send_modify(|s| {
            s.phase = phase;
            s.message = message;
        });
    }

    fn set_persistent(&self, persistent: bool) {
        self.status.send_modify(|s| s.persistent = Some(persistent));
    }

    pub fn viewers(&self) -> usize {
        self.viewers.load(Ordering::SeqCst)
    }
}

/// An attached viewer. Dropping it detaches.
pub struct Viewer {
    term: Arc<TermSession>,
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.term.viewers.fetch_sub(1, Ordering::SeqCst);
    }
}

// ──────────────────────────────── manager ───────────────────────────────────

/// Every open terminal's live state, plus what a driver needs to (re)connect.
pub struct TerminalManager {
    db: Db,
    data_dir: PathBuf,
    resolver: Arc<dyn HostResolver>,
    tmux_socket: String,
    sessions: Mutex<HashMap<String, Arc<TermSession>>>,
}

/// How one connection's pump ended.
#[derive(Debug, PartialEq, Eq)]
enum End {
    /// The user closed the terminal.
    Closed,
    /// The remote process exited (shell `exit`, or the tmux client left).
    ShellExited,
    /// The connection went away.
    Lost(String),
}

impl TerminalManager {
    pub fn new(db: Db, data_dir: PathBuf, resolver: Arc<dyn HostResolver>) -> Arc<Self> {
        Self::with_tmux_socket(db, data_dir, resolver, TMUX_SOCKET)
    }

    /// Same, on a specific tmux socket name (tests isolate theirs).
    pub fn with_tmux_socket(
        db: Db,
        data_dir: PathBuf,
        resolver: Arc<dyn HostResolver>,
        tmux_socket: &str,
    ) -> Arc<Self> {
        Arc::new(TerminalManager {
            db,
            data_dir,
            resolver,
            tmux_socket: tmux_socket.to_string(),
            sessions: Mutex::new(HashMap::new()),
        })
    }

    /// A manager with no hosts over its own empty in-memory DB, for test
    /// `AppState`s that never open a terminal.
    #[doc(hidden)]
    pub fn inert() -> Arc<Self> {
        struct NoHosts;
        #[async_trait::async_trait]
        impl HostResolver for NoHosts {
            async fn resolve(&self, _: &str, _: &str) -> Result<ResolvedHost, String> {
                Err("no terminal hosts".into())
            }
            async fn list_hosts(&self) -> Vec<HostEntry> {
                Vec::new()
            }
        }
        Self::new(
            Db::in_memory().expect("in-memory db"),
            std::env::temp_dir(),
            Arc::new(NoHosts),
        )
    }

    pub fn resolver(&self) -> &Arc<dyn HostResolver> {
        &self.resolver
    }

    /// Current status of a terminal; `Idle` (persistence from the DB row)
    /// when nothing is in memory for it.
    pub fn status_of(&self, row: &TerminalRow) -> Status {
        match self
            .sessions
            .lock()
            .expect("terminals poisoned")
            .get(&row.id)
        {
            Some(t) => t.status(),
            None => Status {
                phase: Phase::Idle,
                persistent: known_persistence(row),
                message: None,
            },
        }
    }

    fn session_for(&self, row: &TerminalRow) -> Arc<TermSession> {
        let mut map = self.sessions.lock().expect("terminals poisoned");
        map.entry(row.id.clone())
            .or_insert_with(|| {
                let t = TermSession::new(row);
                if let Some(p) = known_persistence(row) {
                    t.set_persistent(p);
                }
                t
            })
            .clone()
    }

    /// Attach a viewer at `cols`×`rows`: starts (or wakes) the driver when
    /// the shell isn't connected, and asks a live tmux shell to redraw.
    pub fn attach(
        self: &Arc<Self>,
        row: &TerminalRow,
        cols: u16,
        rows: u16,
    ) -> (Arc<TermSession>, Viewer) {
        let term = self.session_for(row);
        term.viewers.fetch_add(1, Ordering::SeqCst);
        let viewer = Viewer { term: term.clone() };
        term.resize(cols, rows);
        let st = term.status();
        if st.phase == Phase::Live {
            if st.persistent == Some(true) {
                term.send(TermCmd::Redraw);
            }
        } else if st.phase != Phase::Ended {
            self.ensure_driver(&term);
        }
        let db = self.db.clone();
        let id = row.id.clone();
        tokio::spawn(async move {
            let _ = db.touch_terminal(&id).await;
        });
        (term, viewer)
    }

    /// Start a fresh shell for an `ended` / `error` terminal (a viewer's
    /// explicit restart).
    pub fn restart(self: &Arc<Self>, term: &Arc<TermSession>) {
        if term.status().phase != Phase::Live {
            self.ensure_driver(term);
        }
    }

    /// Note keyboard activity: bumps `last_active_at`, throttled.
    pub fn note_activity(&self, term: &TermSession) {
        let now = Instant::now();
        {
            let mut last = term.last_touch.lock().expect("terminal touch poisoned");
            if last.is_some_and(|t| now.duration_since(t) < TOUCH_EVERY) {
                return;
            }
            *last = Some(now);
        }
        let db = self.db.clone();
        let id = term.id.clone();
        tokio::spawn(async move {
            let _ = db.touch_terminal(&id).await;
        });
    }

    fn ensure_driver(self: &Arc<Self>, term: &Arc<TermSession>) {
        if term.closing.load(Ordering::SeqCst) {
            return;
        }
        if term
            .driver_running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            // Already running — maybe asleep in backoff; a viewer is here now.
            term.wake.notify_one();
            return;
        }
        tokio::spawn(self.clone().drive(term.clone()));
    }

    /// Close a terminal for good: end the shell (killing its tmux session)
    /// and forget it. The DB row is closed by the caller.
    pub async fn close(self: &Arc<Self>, row: &TerminalRow) {
        let term = self
            .sessions
            .lock()
            .expect("terminals poisoned")
            .remove(&row.id);
        let handled_by_driver = match &term {
            Some(t) => {
                t.closing.store(true, Ordering::SeqCst);
                t.wake.notify_one();
                let sent = t.send(TermCmd::Close);
                t.set_phase(Phase::Ended, Some("closed".into()));
                sent
            }
            None => false,
        };
        if handled_by_driver || !row.persistent {
            return;
        }
        // No live connection: connect just long enough to kill the tmux
        // session, so a closed terminal doesn't leave a shell running
        // on the host forever. Best effort.
        let this = self.clone();
        let row = row.clone();
        tokio::spawn(async move {
            match this.connect(&row.plugin_id, &row.host_id).await {
                Ok(live) => {
                    let _ = this.kill_tmux(&live, &row.tmux_session).await;
                    let _ = live
                        .handle
                        .disconnect(Disconnect::ByApplication, "", "en")
                        .await;
                }
                Err(e) => tracing::warn!("terminal {}: could not kill tmux session: {e}", row.id),
            }
        });
    }

    // ─────────────────────────── the driver ────────────────────────────────

    async fn drive(self: Arc<Self>, term: Arc<TermSession>) {
        let mut attempt = 0usize;
        let mut reconnecting = false;
        loop {
            if term.closing.load(Ordering::SeqCst) {
                break;
            }
            term.set_phase(
                if reconnecting {
                    Phase::Reconnecting
                } else {
                    Phase::Connecting
                },
                None,
            );
            let lost_reason = match self.open_shell(&term).await {
                Ok((live, channel)) => {
                    attempt = 0;
                    let (tx, rx) = mpsc::unbounded_channel();
                    *term.cmd_tx.lock().expect("terminal cmd poisoned") = Some(tx);
                    // Closed while we were connecting: `close` found no
                    // channel to signal, so end this shell ourselves.
                    if term.closing.load(Ordering::SeqCst) {
                        term.send(TermCmd::Close);
                    }
                    term.set_phase(Phase::Live, None);
                    if reconnecting && term.status().persistent == Some(true) {
                        term.send(TermCmd::Redraw);
                    }
                    let end = pump(&term, channel, rx).await;
                    *term.cmd_tx.lock().expect("terminal cmd poisoned") = None;
                    let persistent = term.status().persistent == Some(true);
                    match end {
                        End::Closed => {
                            if persistent {
                                let _ = self.kill_tmux(&live, &term.tmux_session).await;
                            }
                            disconnect(live).await;
                            break;
                        }
                        End::ShellExited => {
                            // A tmux client also exits when it is detached
                            // (another `attach -d`, a server hiccup) — the
                            // session itself may still be there.
                            let alive = persistent
                                && !live.handle.is_closed()
                                && self.tmux_has_session(&live, &term.tmux_session).await;
                            disconnect(live).await;
                            if !alive {
                                term.set_phase(Phase::Ended, Some("shell exited".into()));
                                break;
                            }
                            reconnecting = true;
                            continue;
                        }
                        End::Lost(reason) => {
                            disconnect(live).await;
                            reason
                        }
                    }
                }
                Err(e) => {
                    if !reconnecting {
                        // First connect failed: surface it and stop — the
                        // viewer can retry; a misconfigured host must not
                        // spin.
                        term.set_phase(Phase::Error, Some(e));
                        break;
                    }
                    e
                }
            };
            if term.closing.load(Ordering::SeqCst) {
                break;
            }
            reconnecting = true;
            let note = if term.status().persistent == Some(true) {
                "\r\n\x1b[33m[connection lost \u{2014} reconnecting\u{2026}]\x1b[0m\r\n"
            } else {
                "\r\n\x1b[33m[connection lost \u{2014} a new shell starts on reconnect]\x1b[0m\r\n"
            };
            if attempt == 0 {
                term.push_output(note.as_bytes());
            }
            if term.viewers() == 0 {
                // Nobody watching: park. The next attach reconnects.
                term.set_phase(Phase::Idle, Some(lost_reason));
                break;
            }
            term.set_phase(Phase::Reconnecting, Some(lost_reason));
            let wait = BACKOFF_SECS[attempt.min(BACKOFF_SECS.len() - 1)];
            attempt += 1;
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(wait)) => {}
                _ = term.wake.notified() => {}
            }
        }
        term.driver_running.store(false, Ordering::SeqCst);
        // A viewer may have attached between our last check and clearing
        // the flag; don't strand it on a parked terminal.
        if term.viewers() > 0
            && !term.closing.load(Ordering::SeqCst)
            && term.status().phase == Phase::Idle
        {
            self.ensure_driver(&term);
        }
    }

    /// Resolve the host through its plugin and open an authenticated,
    /// interactive SSH connection. Credentials live only in this call.
    async fn connect(&self, plugin_id: &str, host_id: &str) -> Result<Live, String> {
        let resolved = self.resolver.resolve(plugin_id, host_id).await?;
        let (_, conn) = ssh::parse_conn(&resolved.conn.to_string())?;
        if conn.uses_key_ref() && !resolved.key_ref_allowed {
            return Err(format!(
                "plugin '{plugin_id}' lacks the 'ssh_keys' permission required to use a stored SSH key"
            ));
        }
        let conn = ssh::resolve_key_ref(conn, &self.db, &self.data_dir).await?;
        ssh::connect_interactive(&conn).await
    }

    /// Connect and start the remote shell (inside tmux when available).
    async fn open_shell(
        &self,
        term: &TermSession,
    ) -> Result<(Live, russh::Channel<client::Msg>), String> {
        let live = self.connect(&term.plugin_id, &term.host_id).await?;
        let persistent = matches!(
            exec_capture(&live, "command -v tmux").await,
            Ok((0, out)) if !out.trim().is_empty()
        );
        if term.status().persistent != Some(persistent) {
            term.set_persistent(persistent);
            let _ = self.db.set_terminal_persistent(&term.id, persistent).await;
        }
        let (cols, rows) = term.size();
        let started = async {
            let channel = live
                .handle
                .channel_open_session()
                .await
                .map_err(|e| format!("open channel failed: {e}"))?;
            channel
                .request_pty(
                    true,
                    "xterm-256color",
                    u32::from(cols),
                    u32::from(rows),
                    0,
                    0,
                    &[],
                )
                .await
                .map_err(|e| format!("pty request failed: {e}"))?;
            if persistent {
                channel
                    .exec(
                        true,
                        tmux_attach_script(&self.tmux_socket, &term.tmux_session, cols, rows),
                    )
                    .await
                    .map_err(|e| format!("tmux start failed: {e}"))?;
            } else {
                channel
                    .request_shell(true)
                    .await
                    .map_err(|e| format!("shell request failed: {e}"))?;
            }
            Ok::<_, String>(channel)
        }
        .await;
        match started {
            Ok(channel) => Ok((live, channel)),
            Err(e) => {
                disconnect(live).await;
                Err(e)
            }
        }
    }

    async fn tmux_has_session(&self, live: &Live, name: &str) -> bool {
        let cmd = format!("tmux -L {} has-session -t {name}", self.tmux_socket);
        matches!(exec_capture(live, &cmd).await, Ok((0, _)))
    }

    async fn kill_tmux(&self, live: &Live, name: &str) -> Result<(), String> {
        let cmd = format!("tmux -L {} kill-session -t {name}", self.tmux_socket);
        exec_capture(live, &cmd).await.map(|_| ())
    }
}

/// The remote command that (re)attaches the terminal's tmux session with
/// plain-shell behaviour. Steps that an old tmux may not know get their own
/// invocation with errors discarded, so they can't abort the rest:
///
/// - `-f /dev/null` + a private socket: never the user's tmux config or
///   sessions; `-u` forces UTF-8 even when the remote locale is unset;
/// - `status off`, `prefix None`: no status bar, no prefix key swallowing
///   Ctrl-b (emacs/readline backward-char);
/// - `escape-time 0`: Esc reaches vim instantly;
/// - `mouse on`: apps that ask for the mouse (vim, htop, less) get it, the
///   wheel scrolls the shell's history, and a drag selection is copied to
///   the browser clipboard over OSC 52 (`set-clipboard on`). The right-click
///   menu is unbound — right-click pastes in the browser instead;
/// - `Tc`: truecolor passes through.
pub(crate) fn tmux_attach_script(socket: &str, session: &str, cols: u16, rows: u16) -> String {
    let t = format!("tmux -u -L {socket}");
    // One invocation sets the long-standing options BEFORE the session's
    // first window exists (so `default-terminal` applies to it); a
    // duplicate `new-session` on reattach fails harmlessly at the end.
    format!(
        "{t} -f /dev/null start-server \\; set -g status off \\; set -g escape-time 0 \\; \
         set -g history-limit 50000 \\; set -g default-terminal screen-256color \\; \
         set -g terminal-overrides 'xterm-256color:Tc' \\; \
         new-session -d -s {session} -x {cols} -y {rows} >/dev/null 2>&1; \
         {t} set -g prefix None >/dev/null 2>&1; \
         {t} set -g prefix2 None >/dev/null 2>&1; \
         {t} set -g mouse on >/dev/null 2>&1; \
         {t} set -g set-clipboard on >/dev/null 2>&1; \
         {t} unbind-key -n MouseDown3Pane >/dev/null 2>&1; \
         exec {t} attach-session -d -t {session}"
    )
}

/// Run a short command on its own channel; `(exit_code, stdout)`.
async fn exec_capture(live: &Live, command: &str) -> Result<(u32, String), String> {
    let run = async {
        let mut channel = live
            .handle
            .channel_open_session()
            .await
            .map_err(|e| format!("open channel failed: {e}"))?;
        channel
            .exec(true, command)
            .await
            .map_err(|e| format!("exec failed: {e}"))?;
        let mut out = Vec::new();
        let mut code = None;
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } => out.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                ChannelMsg::Close => break,
                _ => {}
            }
        }
        Ok((
            code.unwrap_or(255),
            String::from_utf8_lossy(&out).into_owned(),
        ))
    };
    tokio::time::timeout(EXEC_TIMEOUT, run)
        .await
        .map_err(|_| "command timed out".to_string())?
}

/// What the DB row says about tmux. The column only learns `true` for sure:
/// `false` is also the value of a terminal that has never connected, so it
/// reads as unknown until a connect probes the host again.
fn known_persistence(row: &TerminalRow) -> Option<bool> {
    row.persistent.then_some(true)
}

async fn disconnect(live: Live) {
    let _ = live
        .handle
        .disconnect(Disconnect::ByApplication, "terminal detached", "en")
        .await;
}

/// Own the PTY channel for one connection: output to viewers, viewer
/// commands to the channel.
async fn pump(
    term: &TermSession,
    mut channel: russh::Channel<client::Msg>,
    mut cmd_rx: mpsc::UnboundedReceiver<TermCmd>,
) -> End {
    let mut exited = false;
    loop {
        tokio::select! {
            msg = channel.wait() => match msg {
                Some(ChannelMsg::Data { data }) => term.push_output(&data),
                Some(ChannelMsg::ExtendedData { data, .. }) => term.push_output(&data),
                Some(ChannelMsg::ExitStatus { .. }) | Some(ChannelMsg::ExitSignal { .. }) => {
                    exited = true;
                }
                Some(ChannelMsg::Close) => {
                    return if exited { End::ShellExited } else { End::Lost("channel closed".into()) };
                }
                Some(_) => {}
                None => {
                    return if exited { End::ShellExited } else { End::Lost("connection closed".into()) };
                }
            },
            cmd = cmd_rx.recv() => match cmd {
                Some(TermCmd::Input(bytes)) => {
                    if channel.data_bytes(bytes).await.is_err() {
                        return End::Lost("write failed".into());
                    }
                }
                Some(TermCmd::Resize { cols, rows }) => {
                    let _ = channel.window_change(u32::from(cols), u32::from(rows), 0, 0).await;
                }
                Some(TermCmd::Redraw) => {
                    // A size jiggle makes tmux (and full-screen apps) repaint
                    // everything — the cheapest redraw that needs no second
                    // channel.
                    let (cols, rows) = term.size();
                    let _ = channel
                        .window_change(u32::from(cols), u32::from(rows.saturating_sub(1).max(1)), 0, 0)
                        .await;
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    let _ = channel.window_change(u32::from(cols), u32::from(rows), 0, 0).await;
                }
                Some(TermCmd::Close) | None => {
                    let _ = channel.close().await;
                    return End::Closed;
                }
                #[cfg(test)]
                Some(TermCmd::DropConnection) => {
                    return End::Lost("dropped by test".into());
                }
            },
        }
    }
}

#[cfg(test)]
mod tests;
