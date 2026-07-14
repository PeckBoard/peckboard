//! Native SSH client for the `peckboard_ssh_*` host functions.
//!
//! WASM plugins cannot open raw sockets, so the `ssh-fleet` plugin passes the
//! connection details (host, user, password **or** private key) into these
//! host functions on every call and core does the SSH work here. Credentials
//! live only in memory for the duration of a call and are **never logged**.
//!
//! ## Threading
//!
//! Like [`super::host::perform_outbound_http`], the blocking entry points hop
//! onto a dedicated `std::thread` before touching a runtime — a host function
//! may itself be invoked from a Tokio worker, and calling `block_on` there
//! would panic. Unlike the HTTP path (a throwaway current-thread runtime per
//! call), SSH keeps a **process-global multi-thread runtime** alive so pooled
//! connections — and the russh background task that drives each one — survive
//! between calls.
//!
//! ## Pooling
//!
//! Connections are cached by `(host, port, user, auth-fingerprint)` with an
//! idle TTL. A cached handle that has since dropped is transparently
//! reconnected once. Different hosts pool independently, so a fleet-wide
//! command fans out concurrently.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use russh::keys::{PrivateKeyWithHashAlg, decode_secret_key};
use russh::{ChannelMsg, Disconnect, client};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// How long an idle pooled connection is kept before eviction.
const CONN_IDLE_TTL: Duration = Duration::from_secs(120);
/// Hard cap on simultaneously pooled connections (LRU-evicted past this).
const MAX_POOLED: usize = 256;
/// Per-stream capture cap for command output / file reads (matches exec).
const MAX_BYTES: usize = 1024 * 1024;
/// Default / max command timeout (seconds).
const EXEC_DEFAULT_TIMEOUT: u64 = 30;
const EXEC_MAX_TIMEOUT: u64 = 600;
/// Default / max TCP connect + auth timeout (seconds).
const CONNECT_DEFAULT_TIMEOUT: u64 = 15;
const CONNECT_MAX_TIMEOUT: u64 = 120;

// ─────────────────────────────── input parsing ──────────────────────────────

/// Auth material for one connection. Untagged: a `password` object or a
/// `private_key` object. Kept out of `Debug`/logs.
#[derive(Deserialize)]
#[serde(untagged)]
enum Auth {
    Password {
        password: String,
    },
    Key {
        private_key: String,
        #[serde(default)]
        passphrase: Option<String>,
    },
}

/// The connection-shaped fields shared by every `ssh_*` input.
#[derive(Deserialize)]
struct Conn {
    host: String,
    #[serde(default = "default_port")]
    port: u16,
    username: String,
    auth: Auth,
    /// Optional pinned server-key fingerprint (`SHA256:…`). When set, a
    /// mismatch aborts the handshake (TOFU pinning).
    #[serde(default)]
    known_host: Option<String>,
    #[serde(default)]
    connect_timeout_secs: Option<u64>,
}

fn default_port() -> u16 {
    22
}

impl Conn {
    fn validate(&self) -> Result<(), String> {
        if self.host.trim().is_empty() {
            return Err("`host` is required".into());
        }
        if self.username.trim().is_empty() {
            return Err("`username` is required".into());
        }
        Ok(())
    }

    /// A stable pool key that binds the identity **and** the exact credential,
    /// so rotating a password/key forces a fresh connection. The secret is
    /// hashed, never stored in the clear.
    fn pool_key(&self) -> String {
        let mut h = Sha256::new();
        match &self.auth {
            Auth::Password { password } => {
                h.update(b"pw\0");
                h.update(password.as_bytes());
            }
            Auth::Key {
                private_key,
                passphrase,
            } => {
                h.update(b"key\0");
                h.update(private_key.as_bytes());
                if let Some(p) = passphrase {
                    h.update(b"\0");
                    h.update(p.as_bytes());
                }
            }
        }
        let auth_fp = hex::encode(h.finalize());
        format!(
            "{}\u{0}{}\u{0}{}\u{0}{}",
            self.host, self.port, self.username, auth_fp
        )
    }

    fn connect_timeout(&self) -> Duration {
        Duration::from_secs(
            self.connect_timeout_secs
                .unwrap_or(CONNECT_DEFAULT_TIMEOUT)
                .clamp(1, CONNECT_MAX_TIMEOUT),
        )
    }
}

// ─────────────────────────────── host-key check ─────────────────────────────

/// russh client handler. Records the server key fingerprint it saw and, if a
/// pin was supplied, rejects a mismatch.
struct HostKeyHandler {
    expected: Option<String>,
    observed: Arc<Mutex<Option<String>>>,
}

impl client::Handler for HostKeyHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        let fp = server_public_key
            .fingerprint(russh::keys::HashAlg::Sha256)
            .to_string();
        if let Ok(mut slot) = self.observed.lock() {
            *slot = Some(fp.clone());
        }
        match &self.expected {
            Some(pin) => Ok(pin == &fp),
            None => Ok(true), // trust-on-first-use; caller records the fp to pin later
        }
    }
}

// ──────────────────────────────── the pool ──────────────────────────────────

struct Live {
    handle: client::Handle<HostKeyHandler>,
    fingerprint: String,
}

struct Slot {
    inner: tokio::sync::Mutex<Option<Live>>,
    last_used: Mutex<Instant>,
}

impl Slot {
    fn new() -> Self {
        Slot {
            inner: tokio::sync::Mutex::new(None),
            last_used: Mutex::new(Instant::now()),
        }
    }
    fn touch(&self) {
        if let Ok(mut t) = self.last_used.lock() {
            *t = Instant::now();
        }
    }
    fn idle_for(&self, now: Instant) -> Duration {
        self.last_used
            .lock()
            .map(|t| now.saturating_duration_since(*t))
            .unwrap_or_default()
    }
}

struct Pool {
    rt: tokio::runtime::Runtime,
    slots: Mutex<HashMap<String, Arc<Slot>>>,
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("ssh-pool")
            .enable_all()
            .build()
            .expect("build ssh runtime");
        Pool {
            rt,
            slots: Mutex::new(HashMap::new()),
        }
    })
}

/// Get (or create) the slot for a pool key, opportunistically reaping idle and
/// over-cap entries. Never holds the map lock across `.await`.
fn get_slot(key: &str) -> Arc<Slot> {
    let p = pool();
    let mut map = p.slots.lock().expect("ssh pool poisoned");
    let now = Instant::now();
    // Drop connections idle past the TTL — unless they're mid-call (locked).
    map.retain(|_, s| s.inner.try_lock().is_err() || s.idle_for(now) < CONN_IDLE_TTL);
    if let Some(s) = map.get(key) {
        return s.clone();
    }
    // Enforce the cap by evicting the least-recently-used free slot.
    while map.len() >= MAX_POOLED {
        let victim = map
            .iter()
            .filter(|(_, s)| s.inner.try_lock().is_ok())
            .max_by_key(|(_, s)| s.idle_for(now))
            .map(|(k, _)| k.clone());
        match victim {
            Some(k) => {
                map.remove(&k);
            }
            None => break, // everything is busy; let the map briefly exceed the cap
        }
    }
    let slot = Arc::new(Slot::new());
    map.insert(key.to_string(), slot.clone());
    slot
}

/// Open a fresh authenticated session for `conn`.
async fn connect(conn: &Conn) -> Result<Live, String> {
    let observed = Arc::new(Mutex::new(None));
    let handler = HostKeyHandler {
        expected: conn.known_host.clone(),
        observed: observed.clone(),
    };
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(300)),
        ..Default::default()
    });
    let mut handle = tokio::time::timeout(
        conn.connect_timeout(),
        client::connect(config, (conn.host.as_str(), conn.port), handler),
    )
    .await
    .map_err(|_| "connect timed out".to_string())?
    .map_err(|e| format!("connect failed: {e}"))?;

    let ok = match &conn.auth {
        Auth::Password { password } => handle
            .authenticate_password(conn.username.as_str(), password)
            .await
            .map_err(|e| format!("authentication error: {e}"))?
            .success(),
        Auth::Key {
            private_key,
            passphrase,
        } => {
            let key = decode_secret_key(private_key, passphrase.as_deref())
                .map_err(|e| format!("invalid private key: {e}"))?;
            let rsa_hash = handle
                .best_supported_rsa_hash()
                .await
                .map_err(|e| format!("authentication error: {e}"))?
                .flatten();
            handle
                .authenticate_publickey(
                    conn.username.as_str(),
                    PrivateKeyWithHashAlg::new(Arc::new(key), rsa_hash),
                )
                .await
                .map_err(|e| format!("authentication error: {e}"))?
                .success()
        }
    };
    if !ok {
        return Err("authentication failed (bad credentials or key rejected)".into());
    }
    let fingerprint = observed
        .lock()
        .ok()
        .and_then(|s| s.clone())
        .unwrap_or_default();
    Ok(Live {
        handle,
        fingerprint,
    })
}

/// Open a session channel on the pooled connection for `conn`, reconnecting
/// once if the cached handle has gone stale. Returns the channel plus the
/// server fingerprint. The slot lock is released once the channel is open.
async fn open_channel(conn: &Conn) -> Result<(russh::Channel<client::Msg>, String), String> {
    let slot = get_slot(&conn.pool_key());
    let mut guard = slot.inner.lock().await;
    for attempt in 0..2 {
        if guard.is_none() {
            *guard = Some(connect(conn).await?);
        }
        let live = guard.as_ref().expect("just populated");
        match live.handle.channel_open_session().await {
            Ok(ch) => {
                let fp = live.fingerprint.clone();
                slot.touch();
                return Ok((ch, fp));
            }
            Err(e) => {
                *guard = None; // drop the stale handle
                if attempt == 1 {
                    return Err(format!("open channel failed: {e}"));
                }
            }
        }
    }
    unreachable!("loop returns within two attempts")
}

fn append_capped(buf: &mut Vec<u8>, data: &[u8], truncated: &mut bool) {
    if *truncated {
        return;
    }
    let room = MAX_BYTES.saturating_sub(buf.len());
    if data.len() > room {
        buf.extend_from_slice(&data[..room]);
        *truncated = true;
    } else {
        buf.extend_from_slice(data);
    }
}

// ─────────────────────────────── operations ─────────────────────────────────

struct ExecOut {
    exit_code: Option<u32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
    timed_out: bool,
    fingerprint: String,
}

async fn do_exec(conn: &Conn, command: &str, timeout: Duration) -> Result<ExecOut, String> {
    let (mut channel, fingerprint) = open_channel(conn).await?;
    channel
        .exec(true, command.as_bytes())
        .await
        .map_err(|e| format!("exec failed: {e}"))?;

    let mut out = ExecOut {
        exit_code: None,
        stdout: Vec::new(),
        stderr: Vec::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        timed_out: false,
        fingerprint,
    };
    let sleep = tokio::time::sleep(timeout);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => {
                out.timed_out = true;
                let _ = channel.close().await;
                break;
            }
            msg = channel.wait() => {
                let Some(msg) = msg else { break };
                match msg {
                    ChannelMsg::Data { ref data } => {
                        append_capped(&mut out.stdout, data, &mut out.stdout_truncated);
                    }
                    ChannelMsg::ExtendedData { ref data, ext } => {
                        if ext == 1 {
                            append_capped(&mut out.stderr, data, &mut out.stderr_truncated);
                        }
                    }
                    ChannelMsg::ExitStatus { exit_status } => {
                        out.exit_code = Some(exit_status);
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(out)
}

async fn do_read_file(conn: &Conn, path: &str) -> Result<(Vec<u8>, bool, String), String> {
    let (channel, fingerprint) = open_channel(conn).await?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| format!("sftp subsystem failed: {e}"))?;
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| format!("sftp init failed: {e}"))?;
    let mut file = sftp
        .open_with_flags(path, OpenFlags::READ)
        .await
        .map_err(|e| format!("open {path} failed: {e}"))?;

    let mut buf: Vec<u8> = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut chunk)
            .await
            .map_err(|e| format!("read {path} failed: {e}"))?;
        if n == 0 {
            break;
        }
        let room = MAX_BYTES.saturating_sub(buf.len());
        if n > room {
            buf.extend_from_slice(&chunk[..room]);
            truncated = true;
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let _ = file.shutdown().await;
    Ok((buf, truncated, fingerprint))
}

async fn do_write_file(conn: &Conn, path: &str, bytes: &[u8]) -> Result<String, String> {
    let (channel, fingerprint) = open_channel(conn).await?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| format!("sftp subsystem failed: {e}"))?;
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| format!("sftp init failed: {e}"))?;
    let mut file = sftp
        .open_with_flags(
            path,
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
        )
        .await
        .map_err(|e| format!("open {path} for write failed: {e}"))?;
    file.write_all(bytes)
        .await
        .map_err(|e| format!("write {path} failed: {e}"))?;
    file.flush()
        .await
        .map_err(|e| format!("flush {path} failed: {e}"))?;
    let _ = file.shutdown().await;
    Ok(fingerprint)
}

async fn do_probe(conn: &Conn) -> Result<(String, u64), String> {
    let start = Instant::now();
    // A fresh probe should not silently reuse a pooled handle — force one open.
    let live = connect(conn).await?;
    // Prove the channel layer works too, then drop it.
    let _ = live.handle.channel_open_session().await;
    let latency = start.elapsed().as_millis() as u64;
    let _ = live
        .handle
        .disconnect(Disconnect::ByApplication, "", "")
        .await;
    Ok((live.fingerprint, latency))
}

// ─────────────────────────────── entry points ───────────────────────────────

/// Run a `'static` SSH future to completion on the global pool runtime from a
/// dedicated thread, so it is safe even when the caller is on a Tokio worker.
fn block_on<F, T>(fut: F) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, String>> + Send + 'static,
    T: Send + 'static,
{
    let p = pool();
    std::thread::spawn(move || p.rt.block_on(fut))
        .join()
        .map_err(|_| "ssh worker thread panicked".to_string())?
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn parse_conn(input: &str) -> Result<(serde_json::Map<String, Value>, Conn), String> {
    let map: serde_json::Map<String, Value> =
        serde_json::from_str(input).map_err(|e| format!("invalid request json: {e}"))?;
    let conn: Conn = serde_json::from_value(Value::Object(map.clone()))
        .map_err(|e| format!("invalid connection fields: {e}"))?;
    conn.validate()?;
    Ok((map, conn))
}

fn err_json(msg: impl std::fmt::Display) -> String {
    json!({ "error": msg.to_string() }).to_string()
}

/// `peckboard_ssh_probe` — connect + authenticate, returning the server-key
/// fingerprint (for TOFU pinning) and round-trip latency. Does not reuse a
/// pooled connection.
pub(crate) fn probe_impl(input: &str) -> String {
    let (_, conn) = match parse_conn(input) {
        Ok(v) => v,
        Err(e) => return err_json(e),
    };
    let started_at = now_rfc3339();
    let start = Instant::now();
    let owned = ConnOwned::from(&conn);
    match block_on(async move { do_probe(&owned.as_conn()).await }) {
        Ok((fingerprint, latency_ms)) => json!({
            "ok": true,
            "server_fingerprint": fingerprint,
            "latency_ms": latency_ms,
            "started_at": started_at,
            "finished_at": now_rfc3339(),
            "duration_ms": start.elapsed().as_millis() as u64,
        })
        .to_string(),
        Err(e) => err_json(e),
    }
}

/// `peckboard_ssh_exec` — run `command` on the host and capture stdout/stderr
/// (1 MiB/stream cap) and the exit code.
pub(crate) fn exec_impl(input: &str) -> String {
    let (map, conn) = match parse_conn(input) {
        Ok(v) => v,
        Err(e) => return err_json(e),
    };
    let command = match map.get("command").and_then(Value::as_str) {
        Some(c) if !c.is_empty() => c.to_string(),
        _ => return err_json("`command` (non-empty string) is required"),
    };
    let timeout = Duration::from_secs(
        map.get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(EXEC_DEFAULT_TIMEOUT)
            .clamp(1, EXEC_MAX_TIMEOUT),
    );
    let started_at = now_rfc3339();
    let start = Instant::now();
    let owned = ConnOwned::from(&conn);
    match block_on(async move { do_exec(&owned.as_conn(), &command, timeout).await }) {
        Ok(o) => json!({
            "ok": true,
            "exit_code": o.exit_code,
            "stdout": String::from_utf8_lossy(&o.stdout),
            "stderr": String::from_utf8_lossy(&o.stderr),
            "stdout_truncated": o.stdout_truncated,
            "stderr_truncated": o.stderr_truncated,
            "timed_out": o.timed_out,
            "server_fingerprint": o.fingerprint,
            "started_at": started_at,
            "finished_at": now_rfc3339(),
            "duration_ms": start.elapsed().as_millis() as u64,
        })
        .to_string(),
        Err(e) => err_json(e),
    }
}

/// `peckboard_ssh_read_file` — read a remote file over SFTP; returns the bytes
/// base64-encoded (1 MiB cap).
pub(crate) fn read_file_impl(input: &str) -> String {
    let (map, conn) = match parse_conn(input) {
        Ok(v) => v,
        Err(e) => return err_json(e),
    };
    let path = match map.get("path").and_then(Value::as_str) {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => return err_json("`path` (non-empty string) is required"),
    };
    let started_at = now_rfc3339();
    let owned = ConnOwned::from(&conn);
    match block_on(async move { do_read_file(&owned.as_conn(), &path).await }) {
        Ok((bytes, truncated, fingerprint)) => json!({
            "ok": true,
            "content_base64": base64::engine::general_purpose::STANDARD.encode(&bytes),
            "size": bytes.len(),
            "truncated": truncated,
            "server_fingerprint": fingerprint,
            "started_at": started_at,
            "finished_at": now_rfc3339(),
        })
        .to_string(),
        Err(e) => err_json(e),
    }
}

/// `peckboard_ssh_write_file` — write bytes (base64) to a remote file over
/// SFTP, creating/truncating it.
pub(crate) fn write_file_impl(input: &str) -> String {
    let (map, conn) = match parse_conn(input) {
        Ok(v) => v,
        Err(e) => return err_json(e),
    };
    let path = match map.get("path").and_then(Value::as_str) {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => return err_json("`path` (non-empty string) is required"),
    };
    let content_b64 = match map.get("content_base64").and_then(Value::as_str) {
        Some(c) => c.to_string(),
        None => return err_json("`content_base64` (string) is required"),
    };
    let bytes = match base64::engine::general_purpose::STANDARD.decode(content_b64.as_bytes()) {
        Ok(b) => b,
        Err(e) => return err_json(format!("content_base64 is not valid base64: {e}")),
    };
    if bytes.len() > MAX_BYTES {
        return err_json(format!("content exceeds {MAX_BYTES}-byte limit"));
    }
    let started_at = now_rfc3339();
    let owned = ConnOwned::from(&conn);
    let n = bytes.len();
    match block_on(async move { do_write_file(&owned.as_conn(), &path, &bytes).await }) {
        Ok(fingerprint) => json!({
            "ok": true,
            "bytes": n,
            "server_fingerprint": fingerprint,
            "started_at": started_at,
            "finished_at": now_rfc3339(),
        })
        .to_string(),
        Err(e) => err_json(e),
    }
}

/// An owned copy of the connection fields, so the async future handed to a
/// worker thread is `'static` (it cannot borrow the caller's `Conn`).
struct ConnOwned {
    host: String,
    port: u16,
    username: String,
    auth: AuthOwned,
    known_host: Option<String>,
    connect_timeout_secs: Option<u64>,
}
enum AuthOwned {
    Password(String),
    Key(String, Option<String>),
}
impl From<&Conn> for ConnOwned {
    fn from(c: &Conn) -> Self {
        ConnOwned {
            host: c.host.clone(),
            port: c.port,
            username: c.username.clone(),
            auth: match &c.auth {
                Auth::Password { password } => AuthOwned::Password(password.clone()),
                Auth::Key {
                    private_key,
                    passphrase,
                } => AuthOwned::Key(private_key.clone(), passphrase.clone()),
            },
            known_host: c.known_host.clone(),
            connect_timeout_secs: c.connect_timeout_secs,
        }
    }
}
impl ConnOwned {
    fn as_conn(&self) -> Conn {
        Conn {
            host: self.host.clone(),
            port: self.port,
            username: self.username.clone(),
            auth: match &self.auth {
                AuthOwned::Password(p) => Auth::Password {
                    password: p.clone(),
                },
                AuthOwned::Key(k, pp) => Auth::Key {
                    private_key: k.clone(),
                    passphrase: pp.clone(),
                },
            },
            known_host: self.known_host.clone(),
            connect_timeout_secs: self.connect_timeout_secs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_host_or_user_is_rejected() {
        assert!(
            exec_impl(r#"{"username":"u","auth":{"password":"p"},"command":"x"}"#)
                .contains("error")
        );
        assert!(
            exec_impl(r#"{"host":"h","auth":{"password":"p"},"command":"x"}"#).contains("error")
        );
        assert!(
            exec_impl(r#"{"host":"h","username":"u","auth":{"password":"p"}}"#).contains("error"),
            "missing command should error"
        );
    }

    #[test]
    fn pool_key_binds_identity_and_secret() {
        let base = r#"{"host":"h","username":"u","auth":{"password":"p1"}}"#;
        let (_, a) = parse_conn(base).unwrap();
        let (_, b) = parse_conn(r#"{"host":"h","username":"u","auth":{"password":"p2"}}"#).unwrap();
        let (_, c) = parse_conn(r#"{"host":"h","username":"u","auth":{"password":"p1"}}"#).unwrap();
        // Same identity, different password → different pool key.
        assert_ne!(a.pool_key(), b.pool_key());
        // Identical inputs → identical key (pool reuse).
        assert_eq!(a.pool_key(), c.pool_key());
        // The key never leaks the secret.
        assert!(!a.pool_key().contains("p1"));
    }

    #[test]
    fn key_auth_parses_with_passphrase() {
        let (_, conn) = parse_conn(
            r#"{"host":"h","port":2222,"username":"u","auth":{"private_key":"KEY","passphrase":"pp"}}"#,
        )
        .unwrap();
        assert_eq!(conn.port, 2222);
        match conn.auth {
            Auth::Key {
                private_key,
                passphrase,
            } => {
                assert_eq!(private_key, "KEY");
                assert_eq!(passphrase.as_deref(), Some("pp"));
            }
            _ => panic!("expected key auth"),
        }
    }

    #[test]
    fn append_capped_truncates_at_limit() {
        let mut buf = Vec::new();
        let mut trunc = false;
        let big = vec![b'x'; MAX_BYTES + 10];
        append_capped(&mut buf, &big, &mut trunc);
        assert_eq!(buf.len(), MAX_BYTES);
        assert!(trunc);
        // Further appends are no-ops once truncated.
        append_capped(&mut buf, b"more", &mut trunc);
        assert_eq!(buf.len(), MAX_BYTES);
    }

    #[test]
    fn default_port_is_22() {
        let (_, conn) =
            parse_conn(r#"{"host":"h","username":"u","auth":{"password":"p"}}"#).unwrap();
        assert_eq!(conn.port, 22);
    }

    /// Real end-to-end test against a throwaway OpenSSH `sshd` on an ephemeral
    /// port, with host/client keys and config confined to a temp dir. Connects
    /// as the current user by key and exercises exec, probe, host-key pinning,
    /// and SFTP. Skips cleanly (does not fail) when OpenSSH is not installed, so
    /// CI without `sshd` stays green.
    #[test]
    fn end_to_end_against_local_sshd() {
        use base64::Engine as _;
        use serde_json::{Value, json};
        use std::fs;
        use std::net::{TcpListener, TcpStream};
        use std::path::PathBuf;
        use std::process::{Child, Command};
        use std::time::Duration as Dur;

        fn first_existing(cands: &[&str]) -> Option<PathBuf> {
            cands.iter().map(PathBuf::from).find(|p| p.exists())
        }
        macro_rules! skip {
            ($($a:tt)*) => {{ eprintln!("SKIP end_to_end_against_local_sshd: {}", format!($($a)*)); return; }};
        }

        let sshd = match first_existing(&["/usr/sbin/sshd", "/usr/bin/sshd", "/sbin/sshd"]) {
            Some(p) => p,
            None => skip!("sshd not found"),
        };
        let keygen = match first_existing(&["/usr/bin/ssh-keygen", "/bin/ssh-keygen"]) {
            Some(p) => p,
            None => skip!("ssh-keygen not found"),
        };
        let sftp_server = first_existing(&[
            "/usr/lib/openssh/sftp-server",
            "/usr/libexec/openssh/sftp-server",
            "/usr/libexec/sftp-server",
            "/usr/lib/ssh/sftp-server",
        ]);

        let dir = match tempfile::tempdir() {
            Ok(d) => d,
            Err(e) => skip!("tempdir: {e}"),
        };
        let dp = dir.path();
        let hostkey = dp.join("hostkey");
        let clientkey = dp.join("id");
        let authkeys = dp.join("authorized_keys");
        let config = dp.join("sshd_config");
        let logfile = dp.join("sshd.log");

        for path in [&hostkey, &clientkey] {
            let ok = Command::new(&keygen)
                .args(["-t", "ed25519", "-N", "", "-q", "-f"])
                .arg(path)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok {
                skip!("ssh-keygen failed for {}", path.display());
            }
        }
        let pubkey = fs::read(clientkey.with_extension("pub")).unwrap();
        fs::write(&authkeys, &pubkey).unwrap();
        let private_pem = fs::read_to_string(&clientkey).unwrap();

        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();

        let mut cfg = format!(
            "Port {port}\nListenAddress 127.0.0.1\nHostKey {hk}\nAuthorizedKeysFile {ak}\n\
StrictModes no\nUsePAM no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\n\
PubkeyAuthentication yes\nLogLevel ERROR\n",
            hk = hostkey.display(),
            ak = authkeys.display(),
        );
        if let Some(s) = &sftp_server {
            cfg.push_str(&format!("Subsystem sftp {}\n", s.display()));
        }
        fs::write(&config, cfg).unwrap();

        struct Kill(Child);
        impl Drop for Kill {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let _guard = match Command::new(&sshd)
            .arg("-D")
            .arg("-f")
            .arg(&config)
            .arg("-E")
            .arg(&logfile)
            .spawn()
        {
            Ok(c) => Kill(c),
            Err(e) => skip!("sshd spawn failed: {e}"),
        };

        let mut up = false;
        for _ in 0..50 {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                up = true;
                break;
            }
            std::thread::sleep(Dur::from_millis(100));
        }
        if !up {
            let log = fs::read_to_string(&logfile).unwrap_or_default();
            skip!("sshd never accepted on {port}; log:\n{log}");
        }

        let user = std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .unwrap_or_default();
        if user.is_empty() {
            skip!("no USER/LOGNAME in env");
        }

        let base = json!({
            "host": "127.0.0.1",
            "port": port,
            "username": user,
            "auth": { "private_key": private_pem },
            "connect_timeout_secs": 5
        });
        let call =
            |v: &Value| -> Value { serde_json::from_str(&exec_impl(&v.to_string())).unwrap() };

        // A wrong host-key pin must abort the (first, unpooled) handshake.
        let mut wrong_pin = base.clone();
        wrong_pin["known_host"] = json!("SHA256:00000000000000000000000000000000000000000000");
        wrong_pin["command"] = json!("echo nope");
        let pinned = call(&wrong_pin);
        assert!(
            pinned.get("error").is_some(),
            "wrong pin must fail: {pinned}"
        );

        // exec: stdout, stderr, exit code, and a real server fingerprint.
        let mut exec_in = base.clone();
        exec_in["command"] = json!("echo hello-ssh; echo oops 1>&2; exit 0");
        let out = call(&exec_in);
        assert!(out.get("error").is_none(), "exec error: {out}");
        assert_eq!(out["exit_code"], 0, "exec: {out}");
        assert!(
            out["stdout"].as_str().unwrap().contains("hello-ssh"),
            "stdout: {out}"
        );
        assert!(
            out["stderr"].as_str().unwrap().contains("oops"),
            "stderr: {out}"
        );
        let fp = out["server_fingerprint"].as_str().unwrap().to_string();
        assert!(fp.starts_with("SHA256:"), "fingerprint: {fp}");

        // Second exec reuses the pooled connection.
        assert_eq!(
            call(&exec_in)["exit_code"],
            0,
            "pooled second exec should succeed"
        );

        // A correct pin still connects.
        let mut good_pin = exec_in.clone();
        good_pin["known_host"] = json!(fp);
        assert_eq!(
            call(&good_pin)["exit_code"],
            0,
            "correct pin should connect"
        );

        // probe reports the same fingerprint.
        let probe: Value = serde_json::from_str(&probe_impl(&base.to_string())).unwrap();
        assert_eq!(probe["ok"], true, "probe: {probe}");
        assert_eq!(probe["server_fingerprint"].as_str().unwrap(), fp);

        // SFTP write then read round-trips exactly (when sftp-server exists).
        if sftp_server.is_some() {
            let remote = dp.join("written.txt");
            let content = b"content-123\nsecond line\n";
            let mut w = base.clone();
            w["path"] = json!(remote.display().to_string());
            w["content_base64"] = json!(base64::engine::general_purpose::STANDARD.encode(content));
            let wo: Value = serde_json::from_str(&write_file_impl(&w.to_string())).unwrap();
            assert!(wo.get("error").is_none(), "write error: {wo}");
            assert_eq!(wo["bytes"], content.len(), "write bytes: {wo}");

            let mut r = base.clone();
            r["path"] = json!(remote.display().to_string());
            let ro: Value = serde_json::from_str(&read_file_impl(&r.to_string())).unwrap();
            assert!(ro.get("error").is_none(), "read error: {ro}");
            let got = base64::engine::general_purpose::STANDARD
                .decode(ro["content_base64"].as_str().unwrap())
                .unwrap();
            assert_eq!(got, content, "sftp round-trip mismatch");
        }
    }
}
