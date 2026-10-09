//! Interactive SSH terminals — PTY shells a plugin page drives through an
//! xterm.js view, kept alive in core independently of any browser tab.
//!
//! A terminal is opened by a plugin through the `peckboard_ssh_term_open`
//! host function (the plugin resolves its stored host record and hands core
//! the connection fields, exactly like `peckboard_ssh_exec`; the credentials
//! never leave this process). Core connects (a **dedicated**, non-pooled
//! session with keepalives — see [`super::ssh::connect_interactive`]),
//! requests an `xterm-256color` PTY plus a shell, and parks the channel in a
//! driver task on the SSH runtime. The registry here maps the terminal id to
//! that task's handles:
//!
//! - **output** fans out to every attached viewer over a broadcast channel
//!   and is also appended to a bounded scrollback ring ([`SCROLLBACK_CAP`]),
//!   so a viewer that attaches later — the user navigated away and came
//!   back, or opened a second tab — first replays what it missed;
//! - **input** and **resize** are forwarded to the channel;
//! - the shell **ends only on an explicit close or when the remote exits**.
//!   A remote exit is recorded (exit code / reason) and the entry stays, with
//!   its scrollback, until it is closed.
//!
//! Viewers attach over `/ws/terminal` ([`crate::ws::terminal`]), which
//! authenticates with the same one-time plugin-scoped ticket as
//! `/ws/plugin-ui` and only ever attaches a page to a terminal **its own
//! plugin** opened. Nothing here is persisted: a server restart ends every
//! shell (the remote side sees the connection drop), and the page simply
//! shows no open terminals.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Bytes;
use russh::{ChannelMsg, Disconnect, client};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, watch};

use super::ssh::{self, Conn, ConnOwned, Live};
use crate::db::Db;

/// Scrollback kept per terminal (bytes of raw PTY output, escape sequences
/// included). Replayed verbatim into a fresh xterm on attach.
pub const SCROLLBACK_CAP: usize = 1024 * 1024;
/// Hard cap on simultaneously open terminals across all plugins — a backstop
/// against a page opening shells in a loop, far above real use.
const MAX_TERMINALS: usize = 64;
/// Broadcast depth per terminal. A viewer that falls this far behind (a
/// stalled socket during a `cat` of a huge file) gets a `resync` nudge and
/// re-attaches from the scrollback instead of blocking the shell.
const OUTPUT_CHANNEL_CAP: usize = 4096;
/// PTY geometry bounds (the client sends whatever xterm fitted to the pane).
const MIN_COLS: u64 = 2;
const MAX_COLS: u64 = 1000;
const MIN_ROWS: u64 = 1;
const MAX_ROWS: u64 = 500;
const DEFAULT_COLS: u64 = 80;
const DEFAULT_ROWS: u64 = 24;

/// Why and how a shell ended.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ExitInfo {
    /// The remote process's exit status, when the server reported one.
    pub code: Option<u32>,
    /// `shell exited`, `closed` (explicit close), `connection closed`, …
    pub reason: String,
}

/// The secret-free view of a terminal handed to plugins and pages.
#[derive(Clone, Debug, serde::Serialize)]
pub struct TerminalInfo {
    pub id: String,
    pub plugin_id: String,
    /// Display label (the plugin passes its host record's label).
    pub label: String,
    /// `user@host:port` — identity only, never the credential.
    pub host: String,
    /// Opaque plugin-side reference (the host record id) so the page can
    /// group terminals per host. Not interpreted by core.
    pub host_id: Option<String>,
    pub created_at: String,
    pub cols: u16,
    pub rows: u16,
    pub exited: Option<ExitInfo>,
}

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

/// Commands the viewers send to the channel driver.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TermCmd {
    Input(Bytes),
    Resize { cols: u16, rows: u16 },
    Close,
}

/// Scrollback and the live fan-out, guarded together so an attach obtains a
/// gap-free `(snapshot, receiver)` pair: no byte can land between the copy
/// and the subscribe.
struct OutState {
    scrollback: Scrollback,
    tx: broadcast::Sender<Bytes>,
}

/// One open terminal. Cheap to share (`Arc`); the channel itself lives in
/// the driver task and is reached only through [`TermCmd`]s.
pub struct Terminal {
    pub id: String,
    pub plugin_id: String,
    pub label: String,
    pub host: String,
    pub host_id: Option<String>,
    pub created_at: String,
    out: Mutex<OutState>,
    cmd_tx: mpsc::UnboundedSender<TermCmd>,
    size: Mutex<(u16, u16)>,
    exit: watch::Sender<Option<ExitInfo>>,
}

impl Terminal {
    fn new(
        id: String,
        plugin_id: String,
        label: String,
        host: String,
        host_id: Option<String>,
        cols: u16,
        rows: u16,
    ) -> (Arc<Terminal>, mpsc::UnboundedReceiver<TermCmd>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (tx, _) = broadcast::channel(OUTPUT_CHANNEL_CAP);
        let (exit, _) = watch::channel(None);
        let term = Arc::new(Terminal {
            id,
            plugin_id,
            label,
            host,
            host_id,
            created_at: chrono::Utc::now().to_rfc3339(),
            out: Mutex::new(OutState {
                scrollback: Scrollback::new(SCROLLBACK_CAP),
                tx,
            }),
            cmd_tx,
            size: Mutex::new((cols, rows)),
            exit,
        });
        (term, cmd_rx)
    }

    /// Attach a viewer: everything so far, plus a receiver for what follows.
    pub fn attach(&self) -> (Vec<u8>, broadcast::Receiver<Bytes>) {
        let out = self.out.lock().expect("terminal output poisoned");
        (out.scrollback.snapshot(), out.tx.subscribe())
    }

    /// Record output from the shell: scrollback + live viewers.
    pub(crate) fn push_output(&self, data: &[u8]) {
        let mut out = self.out.lock().expect("terminal output poisoned");
        out.scrollback.push(data);
        // No receivers is fine — the scrollback is the durable copy.
        let _ = out.tx.send(Bytes::copy_from_slice(data));
    }

    /// Forward keystrokes to the shell. `false` once the driver is gone.
    pub fn input(&self, data: Bytes) -> bool {
        self.cmd_tx.send(TermCmd::Input(data)).is_ok()
    }

    /// Change the PTY geometry (last viewer to resize wins).
    pub fn resize(&self, cols: u16, rows: u16) -> bool {
        if let Ok(mut s) = self.size.lock() {
            *s = (cols, rows);
        }
        self.cmd_tx.send(TermCmd::Resize { cols, rows }).is_ok()
    }

    /// Ask the driver to end the shell (EOF + channel close + disconnect).
    pub fn close_shell(&self) {
        let _ = self.cmd_tx.send(TermCmd::Close);
    }

    pub fn exit_info(&self) -> Option<ExitInfo> {
        self.exit.borrow().clone()
    }

    /// A receiver that resolves `changed()` when the shell ends.
    pub fn exit_rx(&self) -> watch::Receiver<Option<ExitInfo>> {
        self.exit.subscribe()
    }

    pub(crate) fn mark_exited(&self, info: ExitInfo) {
        // Only the first exit counts; a later `Close` on an already-exited
        // shell must not overwrite the real exit code.
        self.exit.send_if_modified(|slot| {
            if slot.is_some() {
                return false;
            }
            *slot = Some(info);
            true
        });
    }

    pub fn info(&self) -> TerminalInfo {
        let (cols, rows) = self.size.lock().map(|s| *s).unwrap_or((0, 0));
        TerminalInfo {
            id: self.id.clone(),
            plugin_id: self.plugin_id.clone(),
            label: self.label.clone(),
            host: self.host.clone(),
            host_id: self.host_id.clone(),
            created_at: self.created_at.clone(),
            cols,
            rows,
            exited: self.exit_info(),
        }
    }
}

/// All open terminals, by id. One process-global instance ([`registry`]);
/// tests build their own.
#[derive(Default)]
pub struct Registry {
    terms: Mutex<HashMap<String, Arc<Terminal>>>,
}

pub fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Registry::default)
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, id: &str) -> Option<Arc<Terminal>> {
        self.terms
            .lock()
            .expect("terminal registry poisoned")
            .get(id)
            .cloned()
    }

    /// Open terminals owned by `plugin_id`, oldest first.
    pub fn list(&self, plugin_id: &str) -> Vec<TerminalInfo> {
        let mut out: Vec<TerminalInfo> = self
            .terms
            .lock()
            .expect("terminal registry poisoned")
            .values()
            .filter(|t| t.plugin_id == plugin_id)
            .map(|t| t.info())
            .collect();
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        out
    }

    /// Close and forget a terminal — only its owning plugin may. The shell
    /// is ended; attached viewers see an `exited` (`closed`) frame.
    pub fn close(&self, plugin_id: &str, id: &str) -> Result<TerminalInfo, String> {
        let removed = {
            let mut terms = self.terms.lock().expect("terminal registry poisoned");
            match terms.get(id) {
                Some(t) if t.plugin_id == plugin_id => terms.remove(id),
                _ => None,
            }
        };
        let Some(term) = removed else {
            return Err(format!("no open terminal '{id}'"));
        };
        term.close_shell();
        // The driver records the real exit; for a detached (test) terminal
        // nobody else will, so make the viewers' state definite here too.
        term.mark_exited(ExitInfo {
            code: None,
            reason: "closed".into(),
        });
        Ok(term.info())
    }

    fn insert(&self, term: Arc<Terminal>) -> Result<(), String> {
        let mut terms = self.terms.lock().expect("terminal registry poisoned");
        if terms.len() >= MAX_TERMINALS {
            return Err(format!(
                "too many open terminals ({MAX_TERMINALS}); close one first"
            ));
        }
        terms.insert(term.id.clone(), term);
        Ok(())
    }

    fn mint_id() -> String {
        format!("t{}", uuid::Uuid::new_v4().simple())
    }

    /// Test seam: register a terminal with no SSH channel behind it. The
    /// returned receiver sees every [`TermCmd`] a viewer would have sent to
    /// the shell; output is injected with [`Terminal::push_output`].
    #[cfg(test)]
    pub(crate) fn insert_detached(
        &self,
        plugin_id: &str,
        label: &str,
    ) -> (Arc<Terminal>, mpsc::UnboundedReceiver<TermCmd>) {
        let (term, rx) = Terminal::new(
            Self::mint_id(),
            plugin_id.to_string(),
            label.to_string(),
            "user@example:22".into(),
            None,
            80,
            24,
        );
        self.insert(term.clone()).expect("under cap");
        (term, rx)
    }
}

// ─────────────────────────────── the driver ─────────────────────────────────

/// Own the PTY channel for the life of the shell: pump output into the
/// terminal, apply viewer commands, and record how it ended. Runs on the SSH
/// runtime so it outlives the host call that opened it.
async fn drive(
    term: Arc<Terminal>,
    live: Live,
    mut channel: russh::Channel<client::Msg>,
    mut cmd_rx: mpsc::UnboundedReceiver<TermCmd>,
) {
    let mut exit_code = None;
    let reason: String;
    loop {
        tokio::select! {
            msg = channel.wait() => {
                match msg {
                    None => { reason = "connection closed".into(); break; }
                    Some(ChannelMsg::Data { data }) => term.push_output(&data),
                    // A PTY merges stderr into the stream; servers that still
                    // send extended data get it shown too.
                    Some(ChannelMsg::ExtendedData { data, .. }) => term.push_output(&data),
                    Some(ChannelMsg::ExitStatus { exit_status }) => exit_code = Some(exit_status),
                    Some(ChannelMsg::ExitSignal { signal_name, .. }) => {
                        reason = format!("killed by signal {signal_name:?}");
                        break;
                    }
                    Some(ChannelMsg::Close) => { reason = "shell exited".into(); break; }
                    // Eof precedes Close; Success/Failure answer our pty/shell
                    // requests; window adjustments are flow control.
                    Some(_) => {}
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(TermCmd::Input(bytes)) => {
                        if channel.data_bytes(bytes).await.is_err() {
                            reason = "write failed".into();
                            break;
                        }
                    }
                    Some(TermCmd::Resize { cols, rows }) => {
                        let _ = channel.window_change(u32::from(cols), u32::from(rows), 0, 0).await;
                    }
                    Some(TermCmd::Close) | None => {
                        let _ = channel.eof().await;
                        let _ = channel.close().await;
                        reason = "closed".into();
                        break;
                    }
                }
            }
        }
    }
    term.mark_exited(ExitInfo {
        code: exit_code,
        reason,
    });
    let _ = live
        .handle
        .disconnect(Disconnect::ByApplication, "terminal closed", "en")
        .await;
}

// ─────────────────────────────── entry points ───────────────────────────────

fn err_json(msg: impl std::fmt::Display) -> String {
    json!({ "error": msg.to_string() }).to_string()
}

fn geometry(map: &serde_json::Map<String, Value>) -> (u16, u16) {
    let cols = map
        .get("cols")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_COLS)
        .clamp(MIN_COLS, MAX_COLS);
    let rows = map
        .get("rows")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_ROWS)
        .clamp(MIN_ROWS, MAX_ROWS);
    (cols as u16, rows as u16)
}

/// `peckboard_ssh_term_open` — connect, request a PTY + shell, and register
/// the terminal for `plugin_id`. Input is the usual connection fields plus
/// optional `label`, `host_id`, `cols`, `rows`. Returns `{ok, terminal}`.
pub(crate) fn open_impl(
    db: &Db,
    data_dir: &Path,
    plugin_id: &str,
    ssh_keys_granted: bool,
    input: &str,
) -> String {
    open_in(registry(), db, data_dir, plugin_id, ssh_keys_granted, input)
}

fn open_in(
    reg: &'static Registry,
    db: &Db,
    data_dir: &Path,
    plugin_id: &str,
    ssh_keys_granted: bool,
    input: &str,
) -> String {
    let (map, conn) = match ssh::parse_conn(input) {
        Ok(v) => v,
        Err(e) => return err_json(e),
    };
    if conn.uses_key_ref() && !ssh_keys_granted {
        return err_json("plugin lacks the 'ssh_keys' permission required to use a stored SSH key");
    }
    let host_desc = format!("{}@{}:{}", conn.username, conn.host, conn.port);
    let label = map
        .get("label")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| host_desc.clone());
    let host_id = map
        .get("host_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let (cols, rows) = geometry(&map);
    if reg.terms.lock().expect("terminal registry poisoned").len() >= MAX_TERMINALS {
        return err_json(format!(
            "too many open terminals ({MAX_TERMINALS}); close one first"
        ));
    }

    let owned = ConnOwned::from(&conn);
    let db = db.clone();
    let data_dir = data_dir.to_path_buf();
    let opened = ssh::block_on(async move {
        let conn: Conn = ssh::resolve_key_ref(owned.as_conn(), &db, &data_dir).await?;
        let live = ssh::connect_interactive(&conn).await?;
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
        channel
            .request_shell(true)
            .await
            .map_err(|e| format!("shell request failed: {e}"))?;
        Ok((live, channel))
    });
    let (live, channel) = match opened {
        Ok(v) => v,
        Err(e) => return err_json(e),
    };

    let (term, cmd_rx) = Terminal::new(
        Registry::mint_id(),
        plugin_id.to_string(),
        label,
        host_desc,
        host_id,
        cols,
        rows,
    );
    if let Err(e) = reg.insert(term.clone()) {
        // Lost the race for the last slot: end the shell we just started.
        ssh::runtime().spawn(async move {
            let _ = channel.close().await;
            let _ = live
                .handle
                .disconnect(Disconnect::ByApplication, "", "en")
                .await;
        });
        return err_json(e);
    }
    ssh::runtime().spawn(drive(term.clone(), live, channel, cmd_rx));
    json!({ "ok": true, "terminal": term.info() }).to_string()
}

/// `peckboard_ssh_term_list` — the calling plugin's open terminals.
pub(crate) fn list_impl(plugin_id: &str) -> String {
    json!({ "terminals": registry().list(plugin_id) }).to_string()
}

/// `peckboard_ssh_term_close` — end and forget one of the plugin's terminals.
pub(crate) fn close_impl(plugin_id: &str, input: &str) -> String {
    let id = serde_json::from_str::<Value>(input)
        .ok()
        .and_then(|v| v.get("id").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default();
    if id.is_empty() {
        return err_json("`id` (non-empty string) is required");
    }
    match registry().close(plugin_id, &id) {
        Ok(info) => json!({ "ok": true, "terminal": info }).to_string(),
        Err(e) => err_json(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn scrollback_keeps_only_the_newest_cap_bytes() {
        let mut sb = Scrollback::new(8);
        sb.push(b"abc");
        sb.push(b"defgh");
        assert_eq!(sb.snapshot(), b"abcdefgh");
        sb.push(b"ij");
        assert_eq!(sb.snapshot(), b"cdefghij", "oldest bytes evicted");
        sb.push(b"0123456789abc");
        assert_eq!(sb.snapshot(), b"56789abc", "oversized push keeps its tail");
        assert_eq!(sb.len(), 8);
    }

    /// The registry/attach contract the WebSocket relies on: a late viewer
    /// replays the scrollback and then streams live output with no gap;
    /// list/close are scoped to the owning plugin; closing forwards `Close`
    /// to the shell and marks the terminal exited for its viewers.
    #[tokio::test]
    async fn attach_replays_scrollback_then_streams_live_and_close_is_owner_scoped() {
        let reg = Registry::new();
        let (term, mut cmd_rx) = reg.insert_detached("ssh-fleet", "web-1");

        term.push_output(b"$ echo one\r\none\r\n");
        let (snapshot, mut rx) = term.attach();
        assert_eq!(
            snapshot, b"$ echo one\r\none\r\n",
            "late attach replays history"
        );

        term.push_output(b"$ ");
        assert_eq!(
            rx.recv().await.unwrap().as_ref(),
            b"$ ",
            "then streams live"
        );
        assert_eq!(
            term.attach().0,
            b"$ echo one\r\none\r\n$ ",
            "scrollback keeps accumulating"
        );

        // Viewer input/resize reach the shell side in order.
        assert!(term.input(Bytes::from_static(b"ls\n")));
        assert!(term.resize(132, 40));
        assert_eq!(
            cmd_rx.recv().await,
            Some(TermCmd::Input(Bytes::from_static(b"ls\n")))
        );
        assert_eq!(
            cmd_rx.recv().await,
            Some(TermCmd::Resize {
                cols: 132,
                rows: 40
            })
        );
        assert_eq!((term.info().cols, term.info().rows), (132, 40));

        // Listing and closing are per plugin.
        assert_eq!(reg.list("ssh-fleet").len(), 1);
        assert!(reg.list("other-plugin").is_empty());
        assert!(
            reg.close("other-plugin", &term.id).is_err(),
            "foreign plugin can't close"
        );
        assert!(reg.get(&term.id).is_some());

        let mut exit_rx = term.exit_rx();
        assert!(term.exit_info().is_none());
        let closed = reg.close("ssh-fleet", &term.id).unwrap();
        assert_eq!(
            closed.exited.as_ref().map(|e| e.reason.as_str()),
            Some("closed")
        );
        assert!(reg.get(&term.id).is_none());
        assert_eq!(cmd_rx.recv().await, Some(TermCmd::Close));
        tokio::time::timeout(Duration::from_secs(1), exit_rx.changed())
            .await
            .expect("viewers are told")
            .unwrap();
        assert_eq!(
            exit_rx.borrow().as_ref().map(|e| e.reason.as_str()),
            Some("closed")
        );

        // The first recorded exit wins over a later one.
        term.mark_exited(ExitInfo {
            code: Some(0),
            reason: "shell exited".into(),
        });
        assert_eq!(term.exit_info().unwrap().reason, "closed");
    }

    /// Real PTY shell against a throwaway local sshd (skips without OpenSSH):
    /// open → output flows → typed command echoes back → `exit` ends the
    /// shell with its status, and the entry survives until closed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pty_shell_against_local_sshd() {
        let Some(sshd) = ssh::test_support::LocalSshd::spawn() else {
            return;
        };
        let db = crate::db::Db::in_memory().unwrap();
        let mut input = sshd.base_conn();
        input["label"] = json!("local");
        input["host_id"] = json!("h1");
        input["cols"] = json!(100);
        input["rows"] = json!(30);
        let plugin = "ssh-fleet-pty-test";

        let out: Value = serde_json::from_str(&open_impl(
            &db,
            sshd.dir(),
            plugin,
            false,
            &input.to_string(),
        ))
        .unwrap();
        assert!(out.get("error").is_none(), "open: {out}");
        let id = out["terminal"]["id"].as_str().unwrap().to_string();
        assert_eq!(out["terminal"]["label"], "local");
        assert_eq!(out["terminal"]["host_id"], "h1");
        assert_eq!(out["terminal"]["cols"], 100);

        let term = registry().get(&id).expect("registered");
        let (_, mut rx) = term.attach();
        let mut exit_rx = term.exit_rx();

        async fn read_until(
            rx: &mut broadcast::Receiver<Bytes>,
            seen: &mut Vec<u8>,
            needle: &[u8],
        ) -> bool {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
            while tokio::time::Instant::now() < deadline {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Ok(Ok(b)) => {
                        seen.extend_from_slice(&b);
                        if seen.windows(needle.len()).any(|w| w == needle) {
                            return true;
                        }
                    }
                    Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                    _ => break,
                }
            }
            false
        }

        let mut seen = Vec::new();
        // A PTY shell echoes what we type; the marker is computed so the
        // output line differs from the input line.
        assert!(term.input(Bytes::from_static(b"printf 'pty-%s\\n' marker-ok\n")));
        assert!(
            read_until(&mut rx, &mut seen, b"pty-marker-ok").await,
            "shell output: {}",
            String::from_utf8_lossy(&seen)
        );
        assert!(term.resize(120, 40));

        // A late viewer replays everything so far.
        let (snapshot, _) = term.attach();
        assert!(
            snapshot.windows(13).any(|w| w == b"pty-marker-ok"),
            "scrollback replays the marker"
        );
        let listed = registry().list(plugin);
        assert_eq!(listed.len(), 1);
        assert!(listed[0].exited.is_none());

        // Remote exit: recorded, entry kept with its scrollback.
        assert!(term.input(Bytes::from_static(b"exit 3\n")));
        tokio::time::timeout(Duration::from_secs(15), exit_rx.changed())
            .await
            .expect("shell exits")
            .unwrap();
        let exit = exit_rx.borrow().clone().unwrap();
        assert_eq!(exit.code, Some(3), "exit: {exit:?}");
        assert!(registry().get(&id).is_some(), "kept until closed");
        assert!(!term.attach().0.is_empty(), "scrollback survives the exit");

        let closed: Value =
            serde_json::from_str(&close_impl(plugin, &json!({ "id": id }).to_string())).unwrap();
        assert_eq!(closed["ok"], true, "close: {closed}");
        assert!(registry().get(&id).is_none());
        assert!(registry().list(plugin).is_empty());
    }
}
