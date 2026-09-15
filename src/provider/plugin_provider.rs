//! WASM-plugin-backed AI providers.
//!
//! A plugin that declares the `provider.register` hook (plus the
//! `register_provider` permission and the `provider.send` hook) can register
//! an AI provider: its models show up in `/api/models` and MCP `list_models`,
//! and sessions dispatch through the ordinary `ProviderRegistry` lookup.
//!
//! [`PluginProviderAdapter`] runs one **turn per WASM call**. The plugin
//! streams [`ProviderEvent`]s through `peckboard_emit_provider_event`.
//! CLI children: `peckboard_provider_spawn` / `_read_line` / `_write_stdin`
//! / `_kill` (cwd pinned to the session folder). HTTP: `peckboard_http_request`.
//! MCP tools: `peckboard_provider_get_mcp_config` (schemas) and
//! `peckboard_provider_invoke_mcp` (dispatch).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use serde::Deserialize;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use crate::db::Db;
use crate::plugin::manager::PluginManager;
use crate::provider::agent::{AgentProvider, ProcessCompletion, SendMessageContext, emit_event};
use crate::provider::message::UserMessage;
#[cfg(test)]
use crate::provider::registry::{AnswerTransport, InterruptKind};
use crate::provider::registry::{EffortLevel, ProviderCapabilities, split_model_account};
use crate::provider::stream::{CrashKind, ModelInfo, ProviderEvent};
use crate::provider::turn::compose_system_prompt;
use crate::ws::broadcaster::Broadcaster;

/// What a plugin hands `peckboard_register_provider`: the provider identity
/// and model catalog core registers on its behalf. Deserialized straight from
/// the host-function input, then validated by [`validate_registration`].
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderRegistration {
    /// Provider id — the prefix in `provider:model` ids. `[a-z0-9_-]`, and
    /// must not collide with an already-registered provider.
    pub id: String,
    pub display_name: String,
    pub models: Vec<ModelInfo>,
    #[serde(default)]
    pub effort_levels: Vec<EffortLevel>,
    /// Published prices per model id, in USD per million tokens. Backs the
    /// adapter's [`AgentProvider::model_price`]; models absent here price as
    /// unknown (never free).
    #[serde(default)]
    pub pricing: HashMap<String, ModelPricing>,
    /// Declared capabilities (optional — older plugins simply omit it and
    /// get [`ProviderCapabilities::plugin_defaults`]). Transport defaults
    /// stay conservative (cooperative interrupt, answers as a new turn);
    /// a plugin that actually implements stdin / hard-kill / a CLI child
    /// declares those and they are honored. Mid-stream injection is the
    /// one transport bit that cannot be turned on through a copied
    /// `capabilities` blob — it is read from the dedicated flag below.
    pub capabilities: Option<ProviderCapabilities>,
    /// The plugin's `provider.send` turn can absorb a SECOND user message
    /// while it is still running: core hands it over through
    /// `peckboard_provider_take_message` instead of persisting it in
    /// `queued_messages` (see `AgentProvider::supports_mid_stream_injection`).
    ///
    /// Defaults to `false`, which is what every plugin registered before this
    /// field existed gets — core keeps using the durable queue for them. Only
    /// declare `true` if the turn actually polls for injected messages;
    /// otherwise a mid-turn message is accepted and never acted on.
    #[serde(default)]
    pub supports_mid_stream_injection: bool,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ModelPricing {
    pub input_usd_per_mtok: f64,
    pub output_usd_per_mtok: f64,
}

/// The capabilities a plugin registration actually gets: declared values
/// (or conservative defaults when omitted). Mid-stream injection is the
/// one transport bit a plugin CANNOT claim via the `capabilities` blob —
/// it is read from the dedicated
/// [`ProviderRegistration::supports_mid_stream_injection`] flag so it can
/// never be turned on by accident. Stdin answers and interrupt kind ARE
/// honored: the host now has `peckboard_provider_write_stdin` / spawn-kill,
/// so a plugin that implements them (mock:ask, CLI children) must be able
/// to say so.
pub fn effective_capabilities(reg: &ProviderRegistration) -> ProviderCapabilities {
    let mut caps = reg
        .capabilities
        .clone()
        .unwrap_or_else(|| ProviderCapabilities::plugin_defaults(&reg.models));
    caps.supports_mid_stream_injection = reg.supports_mid_stream_injection;
    caps
}

/// Shape-validate a model catalog at REGISTRATION: non-empty, ids usable as
/// the suffix of a `provider:model` id, no duplicates, and no `@` — the
/// account-suffix convention owns that character, so seed ids must be bare.
pub fn validate_models(models: &[ModelInfo]) -> Result<(), String> {
    validate_models_inner(models, false)
}

/// Like [`validate_models`] but tolerating `base@account` scoped variants,
/// which the `provider.models` refresh path adds so each stored account gets
/// its own picker entries (`[Account] Model`). The deleted native providers
/// served these variants directly; the plugin refresh path must accept them
/// too or adding an account silently changes nothing in the catalog.
pub fn validate_refresh_models(models: &[ModelInfo]) -> Result<(), String> {
    validate_models_inner(models, true)
}

fn validate_models_inner(models: &[ModelInfo], allow_account_scoped: bool) -> Result<(), String> {
    if models.is_empty() {
        return Err("a provider must register at least one model".into());
    }
    let mut seen = std::collections::HashSet::new();
    for m in models {
        // Whitespace breaks everything downstream; `:` is tolerated
        // (model-id parsing splits on the FIRST colon, which the provider
        // prefix owns).
        let (base, suffix) = match m.id.split_once('@') {
            Some(parts) if allow_account_scoped => (parts.0, Some(parts.1)),
            Some(_) => {
                return Err(format!(
                    "model id '{}' is invalid: '@' is reserved for account-scoped variants",
                    m.id
                ));
            }
            None => (m.id.as_str(), None),
        };
        if base.is_empty() || base.chars().any(char::is_whitespace) {
            return Err(format!(
                "model id '{}' is invalid: must be non-empty with no whitespace",
                m.id
            ));
        }
        if let Some(acct) = suffix
            && (acct.is_empty() || acct.contains('@') || acct.chars().any(char::is_whitespace))
        {
            return Err(format!("model id '{}' has an invalid account suffix", m.id));
        }
        if !seen.insert(m.id.as_str()) {
            return Err(format!("duplicate model id '{}'", m.id));
        }
    }
    Ok(())
}

/// Shape-validate a registration payload. Collision with existing providers
/// is checked separately at apply time (`PluginManager::sync_plugin_providers`)
/// where the registry is available.
pub fn validate_registration(reg: &ProviderRegistration) -> Result<(), String> {
    if reg.id.is_empty() || reg.id.len() > 64 {
        return Err("provider id must be 1..=64 characters".into());
    }
    if !reg
        .id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return Err(format!(
            "provider id '{}' is invalid: only [a-z0-9_-] allowed",
            reg.id
        ));
    }
    if reg.display_name.trim().is_empty() {
        return Err("display_name must not be blank".into());
    }
    validate_models(&reg.models)?;
    if reg.effort_levels.iter().any(|e| e.id.trim().is_empty()) {
        return Err("effort level ids must not be blank".into());
    }
    Ok(())
}

/// Terminal state a plugin turn reported through
/// `peckboard_emit_provider_event` before returning.
#[derive(Debug, Clone)]
pub enum Terminal {
    Completed,
    Crashed {
        reason: String,
        /// Whatever the plugin declared, or the classification sniffed
        /// from `reason` when it declared nothing (plugin providers
        /// default to `unknown` on the wire).
        kind: CrashKind,
    },
}

/// Native mock treated `Completed` with `result_meta.error` as a failed
/// turn (`last_turn_error`) so auth-recovery could fire. Same rule here:
/// the event still lands as `agent-end{status: complete}` (the plugin
/// asked for Completed), but the adapter's `ProcessCompletion` reports
/// `completed: false` with the classified kind.
fn terminal_from_completed(result_meta: &serde_json::Value) -> Terminal {
    let Some(err) = result_meta
        .get("error")
        .and_then(|e| e.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Terminal::Completed;
    };
    let kind = result_meta
        .get("errorKind")
        .cloned()
        .and_then(|v| serde_json::from_value::<CrashKind>(v).ok())
        .filter(|k| !matches!(k, CrashKind::Unknown))
        .unwrap_or_else(|| CrashKind::classify(err));
    Terminal::Crashed {
        reason: err.to_string(),
        kind,
    }
}

/// Trusted, host-side snapshot of the session a provider turn is running
/// for. Captured by the adapter at dispatch time from the resolved
/// `SendMessageContext` (never from plugin-supplied ids) and served back to
/// the plugin by `peckboard_provider_get_session` / `_get_mcp_config`.
#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub folder_path: String,
    pub folder_id: String,
    pub card_id: Option<String>,
    pub project_id: Option<String>,
    pub is_worker: bool,
    pub mcp_config_path: Option<String>,
}

/// One in-flight `provider.send` turn.
pub(crate) struct TurnState {
    /// The plugin executing this turn — the ONLY plugin allowed to emit
    /// events into the session while the turn is active.
    pub(crate) plugin_id: String,
    pub(crate) stop: AtomicBool,
    pub(crate) terminal: std::sync::Mutex<Option<Terminal>>,
    pub(crate) db: Db,
    pub(crate) broadcaster: Arc<Broadcaster>,
    /// Runtime handle for `block_on` from the host function. Safe because a
    /// turn's host calls only ever run on the dedicated `spawn_blocking`
    /// thread driving the plugin's `provider.send` call (the per-plugin
    /// mutex serialises all other dispatches to that plugin for the whole
    /// turn), never on an async worker thread.
    pub(crate) rt: tokio::runtime::Handle,
    pub(crate) snapshot: SessionSnapshot,
    pub(crate) injected: std::sync::Mutex<std::collections::VecDeque<serde_json::Value>>,
    pub(crate) stdin_q: std::sync::Mutex<std::collections::VecDeque<String>>,
    /// Plugin manager for MCP tool dispatch during this turn. `None` in
    /// unit tests that only exercise spawn/read_line.
    pub(crate) plugins: Option<Arc<PluginManager>>,
}

/// Host-side state shared between every [`PluginProviderAdapter`] and the
/// provider host functions in `src/plugin/host.rs`: which session has a turn
/// in flight, owned by which plugin, plus the stop flag, the trusted
/// session snapshot, and any CLI child the plugin spawned for that turn.
/// One instance per [`PluginManager`].
#[derive(Default)]
pub struct PluginProviderRuntime {
    turns: std::sync::Mutex<HashMap<String, Arc<TurnState>>>,
    /// CLI children keyed by session_id. At most one per in-flight turn;
    /// dropped (and killed) when the turn ends.
    children: std::sync::Mutex<HashMap<String, ProviderChild>>,
}

/// One host-owned CLI child a plugin spawned for an in-flight turn.
struct ProviderChild {
    child: Child,
    stdin: Option<ChildStdin>,
    /// Lines from the child's stdout. The stdout reader lives on a helper
    /// thread so `read_line_json` can wait with a timeout without holding
    /// the children lock (stdin writes must not block).
    lines_rx: Arc<std::sync::Mutex<std::sync::mpsc::Receiver<StdOutMsg>>>,
    stderr: Arc<std::sync::Mutex<Vec<u8>>>,
    stderr_thread: Option<std::thread::JoinHandle<()>>,
}

enum StdOutMsg {
    Line(String),
    Eof,
}

fn error_json(msg: impl std::fmt::Display) -> String {
    serde_json::json!({ "error": msg.to_string() }).to_string()
}

#[derive(Deserialize)]
struct SessionIdRequest {
    session_id: String,
}

impl PluginProviderRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    fn turn(&self, session_id: &str) -> Option<Arc<TurnState>> {
        self.turns
            .lock()
            .ok()
            .and_then(|m| m.get(session_id).cloned())
    }

    /// The active turn for `session_id`, only if `plugin_id` owns it.
    fn owned_turn(&self, plugin_id: &str, session_id: &str) -> Result<Arc<TurnState>, String> {
        match self.turn(session_id) {
            Some(t) if t.plugin_id == plugin_id => Ok(t),
            Some(_) => Err(format!(
                "session '{session_id}' is running a turn owned by another plugin"
            )),
            None => Err(format!(
                "no provider turn in flight for session '{session_id}'"
            )),
        }
    }

    pub(crate) fn begin_turn(&self, session_id: &str, turn: TurnState) -> Result<(), String> {
        let mut turns = self
            .turns
            .lock()
            .map_err(|_| "provider turn map poisoned".to_string())?;
        if turns.contains_key(session_id) {
            return Err(format!(
                "a provider turn is already in flight for session '{session_id}'"
            ));
        }
        turns.insert(session_id.to_string(), Arc::new(turn));
        Ok(())
    }

    /// Remove the turn and report the terminal event it emitted (if any).
    pub(crate) fn end_turn(&self, session_id: &str) -> Option<Terminal> {
        self.kill_child(session_id);
        let turn = self.turns.lock().ok()?.remove(session_id)?;
        turn.terminal.lock().ok()?.clone()
    }

    pub fn is_active(&self, session_id: &str) -> bool {
        self.turn(session_id).is_some()
    }

    /// Cooperative interrupt: flag the turn so the plugin's next
    /// `peckboard_provider_should_stop` poll returns true. Also kills any
    /// CLI child so a blocking `peckboard_provider_read_line` unblocks.
    pub fn request_stop(&self, session_id: &str) {
        if let Some(turn) = self.turn(session_id) {
            turn.stop.store(true, Ordering::SeqCst);
        }
        self.kill_child(session_id);
    }

    /// Flag every in-flight turn owned by `plugin_id` — used when the plugin
    /// is unloaded/denied so orphaned turns wind down at their next poll.
    pub fn request_stop_for_plugin(&self, plugin_id: &str) {
        let mut ids = Vec::new();
        if let Ok(turns) = self.turns.lock() {
            for (session_id, turn) in turns.iter() {
                if turn.plugin_id == plugin_id {
                    turn.stop.store(true, Ordering::SeqCst);
                    ids.push(session_id.clone());
                }
            }
        }
        for id in ids {
            self.kill_child(&id);
        }
    }

    /// Kill and drop the CLI child for `session_id`, if any. Idempotent.
    fn kill_child(&self, session_id: &str) {
        let Ok(mut children) = self.children.lock() else {
            return;
        };
        if let Some(mut child) = children.remove(session_id) {
            let _ = child.child.kill();
            let _ = child.child.wait();
            if let Some(handle) = child.stderr_thread.take() {
                let _ = handle.join();
            }
        }
    }

    /// Write `text` to the CLI child's stdin for `session_id`. Used by both
    /// the host-fn (`peckboard_provider_write_stdin`) and the adapter's
    /// `write_stdin` (control-response / question answers).
    pub fn write_child_stdin(&self, session_id: &str, text: &str) -> bool {
        {
            let Ok(mut children) = self.children.lock() else {
                return false;
            };
            if let Some(child) = children.get_mut(session_id) {
                if let Some(stdin) = child.stdin.as_mut() {
                    return stdin.write_all(text.as_bytes()).is_ok() && stdin.flush().is_ok();
                }
            }
        }
        let Some(turn) = self.turn(session_id) else {
            return false;
        };
        let Ok(mut q) = turn.stdin_q.lock() else {
            return false;
        };
        q.push_back(text.to_string());
        true
    }
    /// Hand `message` to the turn `plugin_id` is running for `session_id`.
    /// `false` when there is no such turn (it ended between the caller's
    /// check and this call), in which case the caller must fall back to
    /// dispatching a fresh turn rather than dropping the message.
    pub fn queue_injection(
        &self,
        plugin_id: &str,
        session_id: &str,
        message: serde_json::Value,
    ) -> bool {
        let Ok(turn) = self.owned_turn(plugin_id, session_id) else {
            return false;
        };
        let Ok(mut queue) = turn.injected.lock() else {
            return false;
        };
        queue.push_back(message);
        true
    }

    // ── Host-function backends (JSON-string in/out, never panic) ──────

    /// `peckboard_provider_spawn {session_id, command, args?, env?, cwd?}`.
    /// Starts a host-owned CLI child for the in-flight turn. `cwd` must equal
    /// the trusted session folder (or be omitted, in which case that folder
    /// is used). One child per turn; a second spawn is refused until the
    /// current child exits.
    pub fn spawn_json(&self, plugin_id: &str, input: &str) -> String {
        #[derive(Deserialize)]
        struct SpawnRequest {
            session_id: String,
            command: String,
            #[serde(default)]
            args: Vec<String>,
            #[serde(default)]
            env: HashMap<String, String>,
            /// Keys stripped from the inherited environment before `env` is
            /// applied. Account-scoped CLI spawns use this to drop a host
            /// `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` that would otherwise
            /// outrank the injected account credential.
            #[serde(default)]
            env_remove: Vec<String>,
            #[serde(default)]
            cwd: Option<String>,
        }
        let req: SpawnRequest = match serde_json::from_str(input) {
            Ok(r) => r,
            Err(e) => return error_json(format!("invalid spawn request: {e}")),
        };
        if req.command.is_empty() || req.command.contains('\0') || req.command.contains('\n') {
            return error_json("command must be a non-empty path or bare name");
        }
        let turn = match self.owned_turn(plugin_id, &req.session_id) {
            Ok(t) => t,
            Err(e) => return error_json(e),
        };
        if turn.stop.load(Ordering::SeqCst) {
            return error_json("turn is stopping");
        }
        let folder = turn.snapshot.folder_path.clone();
        let cwd = req.cwd.as_deref().unwrap_or(folder.as_str());
        if Path::new(cwd) != Path::new(&folder) {
            return error_json("cwd must equal the session folder");
        }
        {
            let children = match self.children.lock() {
                Ok(c) => c,
                Err(_) => return error_json("provider child map poisoned"),
            };
            if children.contains_key(&req.session_id) {
                return error_json("a CLI child is already running for this turn");
            }
        }
        let command = crate::provider::turn::resolve_cli_path(
            &req.command,
            crate::provider::turn::COMMON_CLI_FALLBACK_DIRS,
        );
        let mut cmd = Command::new(&command);
        cmd.args(&req.args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for k in &req.env_remove {
            if !k.is_empty() && !k.contains('\0') {
                cmd.env_remove(k);
            }
        }
        for (k, v) in &req.env {
            cmd.env(k, v);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return error_json(format!("failed to spawn '{}': {e}", req.command)),
        };
        let stdin = child.stdin.take();
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let (lines_tx, lines_rx) = std::sync::mpsc::channel();
        if let Some(pipe) = stdout_pipe {
            std::thread::spawn(move || {
                let mut reader = BufReader::new(pipe);
                let mut buf = String::new();
                loop {
                    buf.clear();
                    match reader.read_line(&mut buf) {
                        Ok(0) | Err(_) => {
                            let _ = lines_tx.send(StdOutMsg::Eof);
                            break;
                        }
                        Ok(_) => {
                            if buf.ends_with('\n') {
                                buf.pop();
                                if buf.ends_with('\r') {
                                    buf.pop();
                                }
                            }
                            if lines_tx.send(StdOutMsg::Line(buf.clone())).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        } else {
            let _ = lines_tx.send(StdOutMsg::Eof);
        }
        let stderr_buf = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stderr_thread = stderr_pipe.map(|pipe| {
            let buf = stderr_buf.clone();
            std::thread::spawn(move || {
                let mut r = pipe;
                let mut chunk = [0u8; 8192];
                loop {
                    match r.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if let Ok(mut b) = buf.lock() {
                                if b.len() < 16 * 1024 {
                                    let room = 16 * 1024 - b.len();
                                    b.extend_from_slice(&chunk[..n.min(room)]);
                                }
                            }
                        }
                    }
                }
            })
        });
        match self.children.lock() {
            Ok(mut children) => {
                children.insert(
                    req.session_id,
                    ProviderChild {
                        child,
                        stdin,
                        lines_rx: Arc::new(std::sync::Mutex::new(lines_rx)),
                        stderr: stderr_buf,
                        stderr_thread,
                    },
                );
            }
            Err(_) => {
                let _ = child.kill();
                return error_json("provider child map poisoned");
            }
        }
        serde_json::json!({ "ok": true }).to_string()
    }
    /// `peckboard_provider_read_line {session_id, timeout_ms?}` — one stdout
    /// line (without trailing newline), `{eof, exit_code, stderr}` on end, or
    /// `{timeout: true}` if `timeout_ms` elapsed with no line.
    pub fn read_line_json(&self, plugin_id: &str, input: &str) -> String {
        #[derive(Deserialize)]
        struct ReadRequest {
            session_id: String,
            #[serde(default)]
            timeout_ms: Option<u64>,
        }
        let req: ReadRequest = match serde_json::from_str(input) {
            Ok(r) => r,
            Err(e) => return error_json(format!("invalid read_line request: {e}")),
        };
        let turn = match self.owned_turn(plugin_id, &req.session_id) {
            Ok(t) => t,
            Err(e) => return error_json(e),
        };
        let timeout = req.timeout_ms.map(Duration::from_millis);
        let deadline = timeout.map(|d| Instant::now() + d);

        let lines_rx = {
            let children = match self.children.lock() {
                Ok(c) => c,
                Err(_) => return error_json("provider child map poisoned"),
            };
            match children.get(&req.session_id) {
                Some(child) => child.lines_rx.clone(),
                None => return error_json("no CLI child running for this turn"),
            }
        };

        loop {
            if turn.stop.load(Ordering::SeqCst) {
                return serde_json::json!({ "stopped": true }).to_string();
            }
            let wait = match deadline {
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        return serde_json::json!({ "timeout": true }).to_string();
                    }
                    d.saturating_duration_since(now)
                        .min(Duration::from_millis(50))
                }
                None => Duration::from_millis(50),
            };
            let msg = {
                let rx = match lines_rx.lock() {
                    Ok(r) => r,
                    Err(_) => return error_json("stdout channel poisoned"),
                };
                rx.recv_timeout(wait)
            };
            match msg {
                Ok(StdOutMsg::Line(line)) => {
                    return serde_json::json!({ "line": line }).to_string();
                }
                Ok(StdOutMsg::Eof) => {
                    let mut children = match self.children.lock() {
                        Ok(c) => c,
                        Err(_) => return error_json("provider child map poisoned"),
                    };
                    let Some(mut child) = children.remove(&req.session_id) else {
                        return serde_json::json!({
                            "eof": true,
                            "exit_code": serde_json::Value::Null,
                            "stderr": "",
                        })
                        .to_string();
                    };
                    drop(children);
                    let status = child.child.wait();
                    if let Some(handle) = child.stderr_thread.take() {
                        let _ = handle.join();
                    }
                    let exit_code = status.ok().and_then(|s| s.code());
                    let stderr = child
                        .stderr
                        .lock()
                        .ok()
                        .map(|b| String::from_utf8_lossy(b.as_slice()).into_owned())
                        .unwrap_or_default();
                    return serde_json::json!({
                        "eof": true,
                        "exit_code": exit_code,
                        "stderr": stderr,
                    })
                    .to_string();
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return serde_json::json!({
                        "eof": true,
                        "exit_code": serde_json::Value::Null,
                        "stderr": "",
                    })
                    .to_string();
                }
            }
        }
    }

    /// `peckboard_provider_write_stdin {session_id, text}`.
    pub fn write_stdin_json(&self, plugin_id: &str, input: &str) -> String {
        #[derive(Deserialize)]
        struct WriteRequest {
            session_id: String,
            text: String,
        }
        let req: WriteRequest = match serde_json::from_str(input) {
            Ok(r) => r,
            Err(e) => return error_json(format!("invalid write_stdin request: {e}")),
        };
        if let Err(e) = self.owned_turn(plugin_id, &req.session_id) {
            return error_json(e);
        }
        if self.write_child_stdin(&req.session_id, &req.text) {
            serde_json::json!({ "ok": true }).to_string()
        } else {
            error_json("no CLI child stdin for this turn")
        }
    }

    /// `peckboard_provider_read_stdin {session_id, timeout_ms?}` — pop text
    /// `write_stdin` queued when there is no CLI child (mock ask/block).
    pub fn take_stdin_json(&self, plugin_id: &str, input: &str) -> String {
        #[derive(Deserialize)]
        struct ReadStdinRequest {
            session_id: String,
            #[serde(default)]
            timeout_ms: Option<u64>,
        }
        let req: ReadStdinRequest = match serde_json::from_str(input) {
            Ok(r) => r,
            Err(e) => return error_json(format!("invalid read_stdin request: {e}")),
        };
        let turn = match self.owned_turn(plugin_id, &req.session_id) {
            Ok(t) => t,
            Err(e) => return error_json(e),
        };
        let deadline = req
            .timeout_ms
            .map(|ms| Instant::now() + Duration::from_millis(ms));
        loop {
            if turn.stop.load(Ordering::SeqCst) {
                return serde_json::json!({ "stopped": true }).to_string();
            }
            {
                let Ok(mut q) = turn.stdin_q.lock() else {
                    return error_json("turn state poisoned");
                };
                if let Some(text) = q.pop_front() {
                    return serde_json::json!({ "text": text }).to_string();
                }
            }
            if let Some(d) = deadline {
                if Instant::now() >= d {
                    return serde_json::json!({ "timeout": true }).to_string();
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// `peckboard_provider_kill {session_id}`.
    pub fn kill_json(&self, plugin_id: &str, input: &str) -> String {
        let req: SessionIdRequest = match serde_json::from_str(input) {
            Ok(r) => r,
            Err(e) => return error_json(format!("invalid kill request: {e}")),
        };
        if let Err(e) = self.owned_turn(plugin_id, &req.session_id) {
            return error_json(e);
        }
        self.kill_child(&req.session_id);
        serde_json::json!({ "ok": true }).to_string()
    }

    /// `peckboard_emit_provider_event {session_id, event}` — validate the
    /// caller owns the session's active turn, then feed the event through the
    /// shared `emit_event` path (DB + usage + WS). Records Completed/Crashed
    /// as the turn's terminal; further emits after a terminal are refused.
    pub fn emit_from_plugin(&self, plugin_id: &str, input: &str) -> String {
        #[derive(Deserialize)]
        struct EmitRequest {
            session_id: String,
            event: serde_json::Value,
        }
        let req: EmitRequest = match serde_json::from_str(input) {
            Ok(r) => r,
            Err(e) => return error_json(format!("invalid emit request: {e}")),
        };
        let turn = match self.owned_turn(plugin_id, &req.session_id) {
            Ok(t) => t,
            Err(e) => return error_json(e),
        };
        let event: ProviderEvent = match serde_json::from_value(req.event) {
            Ok(ev) => ev,
            Err(e) => return error_json(format!("invalid provider event: {e}")),
        };
        {
            let Ok(mut terminal) = turn.terminal.lock() else {
                return error_json("turn state poisoned");
            };
            if terminal.is_some() {
                return error_json("turn already ended (Completed/Crashed was emitted)");
            }
            match &event {
                ProviderEvent::Completed { result_meta, .. } => {
                    *terminal = Some(terminal_from_completed(result_meta));
                }
                ProviderEvent::Crashed {
                    reason, error_kind, ..
                } => {
                    *terminal = Some(Terminal::Crashed {
                        reason: reason.clone(),
                        kind: if matches!(error_kind, CrashKind::Unknown) {
                            CrashKind::classify(reason)
                        } else {
                            *error_kind
                        },
                    })
                }
                _ => {}
            }
        }
        turn.rt.block_on(emit_event(
            &turn.db,
            &turn.broadcaster,
            &req.session_id,
            event,
        ));
        serde_json::json!({ "ok": true }).to_string()
    }

    /// `peckboard_provider_should_stop {session_id}` — the cooperative
    /// interrupt flag. Sessions without an owned active turn read as
    /// `stop: true` so an orphaned or confused plugin winds down.
    pub fn should_stop_json(&self, plugin_id: &str, input: &str) -> String {
        let req: SessionIdRequest = match serde_json::from_str(input) {
            Ok(r) => r,
            Err(e) => return error_json(format!("invalid request: {e}")),
        };
        let stop = match self.owned_turn(plugin_id, &req.session_id) {
            Ok(turn) => turn.stop.load(Ordering::SeqCst),
            Err(_) => true,
        };
        serde_json::json!({ "stop": stop }).to_string()
    }

    /// `peckboard_provider_get_session {session_id}` — the trusted session
    /// snapshot captured at dispatch time.
    /// `peckboard_provider_take_message {session_id}` — pop the oldest user
    /// message core handed to this turn mid-flight, or `{"message": null}`
    /// when there is none. Poll it where the turn can act on more input (a
    /// plugin that never polls must not declare
    /// [`ProviderRegistration::supports_mid_stream_injection`], since core
    /// then skips the durable queue for it).
    pub fn take_message_json(&self, plugin_id: &str, input: &str) -> String {
        let req: SessionIdRequest = match serde_json::from_str(input) {
            Ok(r) => r,
            Err(e) => return error_json(format!("invalid request: {e}")),
        };
        let turn = match self.owned_turn(plugin_id, &req.session_id) {
            Ok(t) => t,
            Err(e) => return error_json(e),
        };
        let Ok(mut queue) = turn.injected.lock() else {
            return error_json("turn state poisoned");
        };
        match queue.pop_front() {
            Some(message) => serde_json::json!({ "message": message }).to_string(),
            None => serde_json::json!({ "message": serde_json::Value::Null }).to_string(),
        }
    }

    pub fn get_session_json(&self, plugin_id: &str, input: &str) -> String {
        let req: SessionIdRequest = match serde_json::from_str(input) {
            Ok(r) => r,
            Err(e) => return error_json(format!("invalid request: {e}")),
        };
        match self.owned_turn(plugin_id, &req.session_id) {
            Ok(turn) => serde_json::json!({
                "session_id": req.session_id,
                "folder_path": turn.snapshot.folder_path,
                "folder_id": turn.snapshot.folder_id,
                "card_id": turn.snapshot.card_id,
                "project_id": turn.snapshot.project_id,
                "is_worker": turn.snapshot.is_worker,
            })
            .to_string(),
            Err(e) => error_json(e),
        }
    }

    /// `peckboard_provider_get_mcp_config {session_id}` — path of the
    /// per-session MCP config core already writes (`worker-mcp/<sid>.json`),
    /// as resolved on the dispatch's `SpawnConfig`. `null` when the session
    /// was dispatched without one.
    pub fn get_mcp_config_json(&self, plugin_id: &str, input: &str) -> String {
        let req: SessionIdRequest = match serde_json::from_str(input) {
            Ok(r) => r,
            Err(e) => return error_json(format!("invalid request: {e}")),
        };
        match self.owned_turn(plugin_id, &req.session_id) {
            Ok(turn) => {
                let path = turn.snapshot.mcp_config_path.clone();
                let contents = path.as_ref().and_then(|p| std::fs::read_to_string(p).ok());
                let hidden: &[&str] = if turn.snapshot.is_worker {
                    crate::service::mcp_server::worker_hidden_tool_names()
                } else {
                    crate::service::mcp_server::chat_hidden_tool_names()
                };
                let registry = crate::service::mcp_server::McpToolRegistry::new();
                let mut tool_defs: Vec<serde_json::Value> = registry
                    .tool_definitions()
                    .iter()
                    .filter(|t| !hidden.contains(&t.name.as_str()))
                    .map(|t| {
                        serde_json::json!({
                            "name": t.name,
                            "description": t.description,
                            "input_schema": t.input_schema,
                        })
                    })
                    .collect();
                if let Some(plugins) = &turn.plugins {
                    for t in turn.rt.block_on(plugins.mcp_tools()) {
                        if turn.snapshot.is_worker && !t.worker_allowed {
                            continue;
                        }
                        if tool_defs.iter().any(|d| {
                            d.get("name").and_then(|n| n.as_str()) == Some(t.name.as_str())
                        }) {
                            continue;
                        }
                        tool_defs.push(serde_json::json!({
                            "name": t.name,
                            "description": t.description,
                            "input_schema": t.input_schema,
                        }));
                    }
                }
                serde_json::json!({
                    "path": path,
                    "contents": contents,
                    "core_tools": crate::service::mcp_server::tool_names(),
                    "pre_hatcher_tools": crate::service::mcp_server::pre_hatcher_allowed_tool_names(),
                    "tool_defs": tool_defs,
                })
                .to_string()
            }
            Err(e) => error_json(e),
        }
    }

    /// `peckboard_provider_account_env {session_id, model?}` — env the CLI
    /// child must inherit to authenticate as the session's stored account.
    /// Empty `env` when the model has no `@account` suffix (Default/host
    /// credentials). Missing account id is a hard error.
    pub fn account_env_json(&self, plugin_id: &str, input: &str) -> String {
        #[derive(Deserialize)]
        struct AccountEnvRequest {
            session_id: String,
            #[serde(default)]
            model: Option<String>,
        }
        let req: AccountEnvRequest = match serde_json::from_str(input) {
            Ok(r) => r,
            Err(e) => return error_json(format!("invalid account_env request: {e}")),
        };
        let turn = match self.owned_turn(plugin_id, &req.session_id) {
            Ok(t) => t,
            Err(e) => return error_json(e),
        };
        match turn.rt.block_on(account_env_for(
            &turn.db,
            plugin_id,
            &req.session_id,
            req.model.as_deref(),
        )) {
            Ok(v) => v.to_string(),
            Err(e) => error_json(e),
        }
    }
    /// `peckboard_provider_invoke_mcp {session_id, name, arguments}` — run one
    /// MCP tool the same way the `/mcp` route does. Turn-gated: only the
    /// plugin owning this `provider.send` may call it. Result is `{ok, result}`
    /// or `{ok: false, error}` so the plugin can feed a tool error back to the
    /// model instead of aborting the turn.
    pub fn invoke_mcp_json(&self, plugin_id: &str, input: &str) -> String {
        #[derive(Deserialize)]
        struct InvokeReq {
            session_id: String,
            name: String,
            #[serde(default)]
            arguments: serde_json::Value,
        }
        let req: InvokeReq = match serde_json::from_str(input) {
            Ok(r) => r,
            Err(e) => return error_json(format!("invalid invoke_mcp request: {e}")),
        };
        if req.name.trim().is_empty() {
            return error_json("tool name must not be empty");
        }
        let turn = match self.owned_turn(plugin_id, &req.session_id) {
            Ok(t) => t,
            Err(e) => return error_json(e),
        };
        let Some(plugins) = turn.plugins.clone() else {
            return error_json("no plugin manager on this turn");
        };
        let ctx = crate::service::mcp_server::ToolCallContext {
            session_id: req.session_id.clone(),
            project_id: turn.snapshot.project_id.clone(),
            card_id: turn.snapshot.card_id.clone(),
            folder_id: turn.snapshot.folder_id.clone(),
            db: Arc::new(turn.db.clone()),
            broadcaster: turn.broadcaster.clone(),
            provider_registry: plugins.bound_provider_registry(),
            data_dir: Some(plugins.data_dir()),
        };
        let registry = crate::service::mcp_server::McpToolRegistry::new();
        match turn
            .rt
            .block_on(crate::service::mcp_server::dispatch_tool_call(
                &plugins,
                &registry,
                req.name.trim(),
                req.arguments,
                &ctx,
            )) {
            Ok(value) => serde_json::json!({ "ok": true, "result": value }).to_string(),
            Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }).to_string(),
        }
    }

    /// `peckboard_provider_write_file {session_id, path, contents}` — write
    /// a UTF-8 file under the session folder. `path` is relative; `..` and
    /// absolute paths are rejected. Used for workspace MCP config and the
    /// Claude subagent-context file.
    pub fn write_file_json(&self, plugin_id: &str, input: &str) -> String {
        #[derive(Deserialize)]
        struct WriteFileRequest {
            session_id: String,
            path: String,
            contents: String,
        }
        let req: WriteFileRequest = match serde_json::from_str(input) {
            Ok(r) => r,
            Err(e) => return error_json(format!("invalid write_file request: {e}")),
        };
        if req.contents.len() > 1024 * 1024 {
            return error_json("file too large (max 1 MiB)");
        }
        let turn = match self.owned_turn(plugin_id, &req.session_id) {
            Ok(t) => t,
            Err(e) => return error_json(e),
        };
        let folder = Path::new(&turn.snapshot.folder_path);
        let rel = Path::new(&req.path);
        if rel.is_absolute()
            || req.path.is_empty()
            || req.path.contains('\0')
            || rel.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir | std::path::Component::Prefix(_)
                )
            })
        {
            return error_json("path must be a relative file under the session folder");
        }
        let dest = folder.join(rel);
        if let Some(parent) = dest.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return error_json(format!("create_dir: {e}"));
            }
        }
        match std::fs::write(&dest, req.contents.as_bytes()) {
            Ok(()) => serde_json::json!({ "ok": true, "path": dest.to_string_lossy() }).to_string(),
            Err(e) => error_json(format!("write: {e}")),
        }
    }
}

/// TTL cache over one probe invocation, keyed by the raw request (command +
/// args + env). Success and failure are cached alike so a broken or slow CLI
/// stalls at most one catalog request per window — the same discipline the
/// deleted native providers applied to their discovery probes. Settings and
/// account merging happen in the plugin on every `provider.models` call;
/// only the CLI shell-out is memoised here, so a settings or account change
/// still shows up in the catalog immediately.
const PROBE_CACHE_TTL: Duration = Duration::from_secs(60);

static PROBE_CACHE: std::sync::LazyLock<std::sync::Mutex<HashMap<String, (Instant, String)>>> =
    std::sync::LazyLock::new(Default::default);

/// `peckboard_provider_probe {command, args?, env?, timeout_ms?}` — short-lived
/// CLI capture for `provider.models` discovery. Not tied to a turn.
pub fn probe_cli_json(input: &str) -> String {
    if let Some((at, cached)) = PROBE_CACHE.lock().ok().and_then(|c| c.get(input).cloned())
        && at.elapsed() < PROBE_CACHE_TTL
    {
        return cached;
    }
    let out = probe_cli_uncached(input);
    if let Ok(mut cache) = PROBE_CACHE.lock() {
        cache.retain(|_, (at, _)| at.elapsed() < PROBE_CACHE_TTL);
        cache.insert(input.to_string(), (Instant::now(), out.clone()));
    }
    out
}

fn probe_cli_uncached(input: &str) -> String {
    #[derive(Deserialize)]
    struct ProbeRequest {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default)]
        timeout_ms: Option<u64>,
    }
    let req: ProbeRequest = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return error_json(format!("invalid probe request: {e}")),
    };
    if req.command.is_empty() || req.command.contains('\0') || req.command.contains('\n') {
        return error_json("command must be a non-empty path or bare name");
    }
    let command = crate::provider::turn::resolve_cli_path(
        &req.command,
        crate::provider::turn::COMMON_CLI_FALLBACK_DIRS,
    );
    let timeout = Duration::from_millis(req.timeout_ms.unwrap_or(15_000).clamp(1_000, 30_000));
    let mut cmd = Command::new(&command);
    cmd.args(&req.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in &req.env {
        cmd.env(k, v);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return error_json(format!("failed to spawn '{command}': {e}")),
    };
    // Drain both pipes concurrently with the wait: a child that writes more
    // than the pipe buffer (codex's bundled catalog is ~500 KB) blocks on
    // write until someone reads, so reading only after exit deadlocks the
    // probe into its timeout. The reader threads see EOF when the child
    // exits or is killed, so the joins below never hang.
    let stdout_reader = child.stdout.take().map(|mut p| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = p.read_to_end(&mut buf);
            buf
        })
    });
    let stderr_reader = child.stderr.take().map(|mut p| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = p.read_to_end(&mut buf);
            buf
        })
    });
    let started = Instant::now();
    loop {
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return error_json("probe timed out");
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = stdout_reader
                    .and_then(|h| h.join().ok())
                    .unwrap_or_default();
                let mut stderr = stderr_reader
                    .and_then(|h| h.join().ok())
                    .unwrap_or_default();
                stdout.truncate(256 * 1024);
                stderr.truncate(16 * 1024);
                return serde_json::json!({
                    "ok": true,
                    "exit_code": status.code(),
                    "stdout": String::from_utf8_lossy(&stdout),
                    "stderr": String::from_utf8_lossy(&stderr),
                })
                .to_string();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return error_json(format!("wait: {e}")),
        }
    }
}

/// `peckboard_provider_list_accounts` — stored accounts for this plugin's
/// provider, used to stamp `@account` catalog variants.
pub fn list_accounts_json(db: &Db, plugin_id: &str) -> String {
    let plugin_id = plugin_id.to_string();
    let db = db.clone();
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        rt.block_on(async move {
            let accounts: Vec<serde_json::Value> = match plugin_id.as_str() {
                "claude" => db
                    .list_claude_accounts()
                    .await
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .map(|a| serde_json::json!({ "id": a.id, "name": a.name }))
                    .collect(),
                "grok" => db
                    .list_grok_accounts()
                    .await
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .map(|a| serde_json::json!({ "id": a.id, "name": a.name }))
                    .collect(),
                "kimi" => db
                    .list_kimi_accounts()
                    .await
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .map(|a| serde_json::json!({ "id": a.id, "name": a.name }))
                    .collect(),
                "codex" => db
                    .list_codex_accounts()
                    .await
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .map(|a| serde_json::json!({ "id": a.id, "name": a.name }))
                    .collect(),
                _ => Vec::new(),
            };
            Ok::<_, String>(serde_json::json!({ "accounts": accounts }).to_string())
        })
    });
    match handle.join() {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => error_json(e),
        Err(_) => error_json("account list thread panicked"),
    }
}

/// The `message` object a turn sees: the user's text plus base64 attachments.
/// Shared by the `provider.send` payload and the mid-stream injection queue so
/// an injected message is byte-for-byte the shape the plugin already parses.
pub(crate) fn message_payload(message: &UserMessage) -> serde_json::Value {
    use base64::Engine as _;
    let attachments: Vec<serde_json::Value> = message
        .attachments
        .iter()
        .map(|a| {
            serde_json::json!({
                "filename": a.filename,
                "mime_type": a.mime_type,
                "data_base64": base64::engine::general_purpose::STANDARD.encode(&a.data),
            })
        })
        .collect();
    serde_json::json!({ "text": message.text, "attachments": attachments })
}

/// Resolve CLI env for `plugin_id` + optional `@account` on `model`.
/// Proof token: caller holds an in-flight turn (see `account_env_json`).
async fn account_env_for(
    db: &Db,
    plugin_id: &str,
    session_id: &str,
    model: Option<&str>,
) -> Result<serde_json::Value, String> {
    let model = match model {
        Some(m) if !m.is_empty() => m.to_string(),
        _ => db
            .get_session(session_id)
            .await
            .map_err(|e| e.to_string())?
            .and_then(|s| s.model)
            .unwrap_or_default(),
    };
    let (_base, account_id) = split_model_account(&model);
    let Some(account_id) = account_id else {
        return Ok(serde_json::json!({ "env": {}, "env_remove": [] }));
    };

    let mut env = HashMap::<String, String>::new();
    let mut env_remove: Vec<String> = Vec::new();

    match plugin_id {
        "claude" => {
            let account = db
                .get_claude_account(account_id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("claude account not found: {account_id}"))?;
            env_remove.extend(["ANTHROPIC_API_KEY".into(), "CLAUDE_CODE_OAUTH_TOKEN".into()]);
            match account.kind.as_str() {
                "api_key" => {
                    env.insert("ANTHROPIC_API_KEY".into(), account.credential.clone());
                }
                "oauth_token" => {
                    let token =
                        crate::accounts::claude_token_refresh::fresh_credential(db, &account)
                            .await
                            .map_err(|e| e.to_string())?;
                    env.insert("CLAUDE_CODE_OAUTH_TOKEN".into(), token);
                }
                other => return Err(format!("unknown claude account kind: {other}")),
            }
            if let Some(dir) = &account.config_dir {
                std::fs::create_dir_all(dir).ok();
                env.insert("CLAUDE_CONFIG_DIR".into(), dir.clone());
            }
        }
        "grok" => {
            let account = db
                .get_grok_account(account_id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("grok account not found: {account_id}"))?;
            if let Some(dir) = &account.config_dir {
                std::fs::create_dir_all(dir).ok();
                env.insert("GROK_HOME".into(), dir.clone());
            }
            if account.kind == "api_key" {
                env.insert("XAI_API_KEY".into(), account.credential.clone());
            }
        }
        "kimi" => {
            let account = db
                .get_kimi_account(account_id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("kimi account not found: {account_id}"))?;
            if let Some(dir) = &account.config_dir {
                std::fs::create_dir_all(dir).ok();
                env.insert("KIMI_CODE_HOME".into(), dir.clone());
            }
            if account.kind == "api_key" {
                env.insert("KIMI_API_KEY".into(), account.credential.clone());
            }
        }
        "codex" => {
            let account = db
                .get_codex_account(account_id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("codex account not found: {account_id}"))?;
            if let Some(dir) = &account.config_dir {
                std::fs::create_dir_all(dir).ok();
                env.insert("CODEX_HOME".into(), dir.clone());
            }
            env_remove.extend(["CODEX_API_KEY".into(), "OPENAI_API_KEY".into()]);
        }
        _ => {}
    }

    Ok(serde_json::json!({ "env": env, "env_remove": env_remove }))
}

/// [`AgentProvider`] registered on behalf of a WASM plugin. Bridges every
/// trait call to the plugin's `provider.send` hook / host-side turn state.
pub struct PluginProviderAdapter {
    provider_id: String,
    plugin_id: String,
    manager: Arc<PluginManager>,
    runtime: Arc<PluginProviderRuntime>,
    /// model id → (input, output) USD per million tokens.
    pricing: HashMap<String, (f64, f64)>,
    /// Whether the registration declared mid-stream injection — mirrored
    /// into [`AgentProvider::supports_mid_stream_injection`].
    mid_stream: bool,
}

impl PluginProviderAdapter {
    pub fn new(
        registration: &ProviderRegistration,
        plugin_id: String,
        manager: Arc<PluginManager>,
        runtime: Arc<PluginProviderRuntime>,
    ) -> Self {
        Self {
            provider_id: registration.id.clone(),
            plugin_id,
            manager,
            runtime,
            mid_stream: registration.supports_mid_stream_injection,
            pricing: registration
                .pricing
                .iter()
                .map(|(m, p)| (m.clone(), (p.input_usd_per_mtok, p.output_usd_per_mtok)))
                .collect(),
        }
    }

    /// The plugin this adapter dispatches to.
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }
}

#[async_trait]
impl AgentProvider for PluginProviderAdapter {
    fn id(&self) -> &str {
        &self.provider_id
    }

    fn model_price(&self, model_id: &str) -> Option<(f64, f64)> {
        self.pricing.get(model_id).copied()
    }

    async fn dynamic_models(&self) -> Option<Vec<ModelInfo>> {
        self.manager.dispatch_provider_models(&self.plugin_id).await
    }

    async fn send_message(&self, ctx: SendMessageContext) -> anyhow::Result<()> {
        // Mid-stream injection: this adapter promised the SessionManager it
        // would absorb a message sent while a turn is running, so core did
        // NOT persist it in `queued_messages`. Hand it to the live turn
        // instead of trying to begin a second one (which `begin_turn` would
        // refuse) — dropping it here would lose the user's message outright.
        if self.mid_stream
            && self.runtime.queue_injection(
                &self.plugin_id,
                &ctx.session_id,
                message_payload(&ctx.message),
            )
        {
            return Ok(());
        }

        let session = ctx
            .db
            .get_session(&ctx.session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("session not found: {}", ctx.session_id))?;
        let snapshot = SessionSnapshot {
            folder_path: ctx.config.working_dir.clone(),
            folder_id: session.folder_id.clone(),
            card_id: session.card_id,
            project_id: session.project_id,
            is_worker: ctx.config.is_worker,
            mcp_config_path: ctx.config.mcp_config_path.clone(),
        };
        self.runtime
            .begin_turn(
                &ctx.session_id,
                TurnState {
                    plugin_id: self.plugin_id.clone(),
                    stop: AtomicBool::new(false),
                    terminal: std::sync::Mutex::new(None),
                    db: ctx.db.clone(),
                    broadcaster: ctx.broadcaster.clone(),
                    rt: tokio::runtime::Handle::current(),
                    snapshot,
                    injected: std::sync::Mutex::new(std::collections::VecDeque::new()),
                    stdin_q: Default::default(),
                    plugins: Some(self.manager.clone()),
                },
            )
            .map_err(|e| anyhow::anyhow!(e))?;

        let payload = serde_json::json!({
            "session_id": ctx.session_id,
            "provider_id": self.provider_id,
            "spawn_config": ctx.config,
            "message": message_payload(&ctx.message),
            "conversation_id": ctx.conversation_id.as_ref().map(|h| h.id()),
            "system_prompt": compose_system_prompt(&ctx.config),
        });

        let manager = self.manager.clone();
        let runtime = self.runtime.clone();
        let plugin_id = self.plugin_id.clone();
        let session_id = ctx.session_id.clone();
        let db = ctx.db.clone();
        let broadcaster = ctx.broadcaster.clone();
        let completion_tx = ctx.completion_tx.clone();
        let run_id = ctx.run_id;
        tokio::spawn(async move {
            let result = manager.dispatch_provider_send(&plugin_id, payload).await;
            let terminal = runtime.end_turn(&session_id);
            let (completed, error, error_kind) = match terminal {
                // The plugin already reported how the turn ended; a trap
                // AFTER a terminal event doesn't retroactively fail it.
                Some(Terminal::Completed) => (true, None, None),
                Some(Terminal::Crashed { reason, kind }) => (false, Some(reason), Some(kind)),
                None => {
                    let reason = match result {
                        Err(e) => e,
                        Ok(()) => "plugin provider returned without emitting Completed or Crashed"
                            .to_string(),
                    };
                    let error_kind = CrashKind::classify(&reason);
                    emit_event(
                        &db,
                        &broadcaster,
                        &session_id,
                        ProviderEvent::Crashed {
                            reason: reason.clone(),
                            error_kind,
                            exit_code: None,
                            stderr: None,
                        },
                    )
                    .await;
                    (false, Some(reason), Some(error_kind))
                }
            };
            let _ = completion_tx
                .send(ProcessCompletion {
                    session_id,
                    completed,
                    error,
                    run_id,
                    error_kind,
                    turn_end_only: false,
                })
                .await;
        });
        Ok(())
    }

    async fn cancel(&self, session_id: &str) {
        self.runtime.request_stop(session_id);
    }

    async fn interrupt(&self, session_id: &str) {
        // Cooperative: the plugin polls `peckboard_provider_should_stop`
        // between HTTP chunks; the per-call WASM timeout is the hard
        // backstop that guarantees the turn terminates regardless. The
        // stop flag goes first — the optional `provider.interrupt` hook is
        // only a cleanup signal and cannot preempt the in-flight call (one
        // extism instance per plugin).
        self.runtime.request_stop(session_id);
        self.manager
            .dispatch_provider_interrupt(&self.plugin_id, session_id, &self.provider_id);
    }

    async fn write_stdin(&self, session_id: &str, text: &str) -> bool {
        self.runtime.write_child_stdin(session_id, text)
    }

    fn supports_mid_stream_injection(&self) -> bool {
        // Declared per registration. `true` ⇒ the SessionManager dispatches a
        // mid-turn message straight here and `send_message` hands it to the
        // live turn's injection queue instead of the DB queue.
        self.mid_stream
    }
    async fn is_running(&self, session_id: &str) -> bool {
        self.runtime.is_active(session_id)
    }

    async fn wait_for_termination(&self, session_id: &str) {
        let wait = async {
            while self.runtime.is_active(session_id) {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        };
        if tokio::time::timeout(std::time::Duration::from_secs(15), wait)
            .await
            .is_err()
        {
            tracing::warn!(session_id, "plugin provider wait_for_termination timed out");
            self.runtime.request_stop(session_id);
        }
    }

    async fn cleanup(&self) {}

    async fn shutdown(&self) {
        self.runtime.request_stop_for_plugin(&self.plugin_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(id: &str) -> ProviderRegistration {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "display_name": "Test",
            "models": [{ "id": "m1", "display_name": "M1" }],
        }))
        .unwrap()
    }

    #[test]
    fn validate_registration_accepts_well_formed() {
        let r: ProviderRegistration = serde_json::from_value(serde_json::json!({
            "id": "acme-ai_2",
            "display_name": "Acme AI",
            "models": [
                { "id": "fast-1", "display_name": "Fast 1", "capabilities": ["reasoning"], "tier": 1 },
                { "id": "slow-1", "display_name": "Slow 1" },
            ],
            "effort_levels": [{ "id": "low", "label": "Low" }],
            "pricing": { "fast-1": { "input_usd_per_mtok": 0.5, "output_usd_per_mtok": 1.5 } },
        }))
        .unwrap();
        assert!(validate_registration(&r).is_ok());
        assert_eq!(r.pricing["fast-1"].output_usd_per_mtok, 1.5);
    }
    #[test]
    fn effective_capabilities_defaults_and_clamps() {
        // Omitted capabilities → conservative defaults, with thinking
        // derived from the model catalog's tags.
        let plain = reg("plain");
        let caps = effective_capabilities(&plain);
        assert!(!caps.supports_thinking);
        assert!(caps.supports_images_in);
        assert!(!caps.supports_usage);
        assert!(caps.supports_resume);
        assert_eq!(caps.interrupt_kind, InterruptKind::Cooperative);
        assert_eq!(caps.answer_transport, AnswerTransport::NewTurn);

        let thinking: ProviderRegistration = serde_json::from_value(serde_json::json!({
            "id": "th",
            "display_name": "Th",
            "models": [{ "id": "m1", "display_name": "M1", "capabilities": ["reasoning"] }],
        }))
        .unwrap();
        assert!(effective_capabilities(&thinking).supports_thinking);

        // Declared capabilities are honored for interrupt/stdin; mid-stream
        // injection still requires the dedicated top-level flag.
        let declared: ProviderRegistration = serde_json::from_value(serde_json::json!({
            "id": "decl",
            "display_name": "Decl",
            "models": [{ "id": "m1", "display_name": "M1" }],
            "capabilities": {
                "supports_images_in": false,
                "supports_usage": true,
                "interrupt_kind": "soft",
                "supports_mid_stream_injection": true,
                "answer_transport": "stdin",
            },
        }))
        .unwrap();
        let caps = effective_capabilities(&declared);
        assert!(!caps.supports_images_in);
        assert!(caps.supports_usage);
        assert_eq!(caps.interrupt_kind, InterruptKind::Soft);
        assert!(!caps.supports_mid_stream_injection);
        assert_eq!(caps.answer_transport, AnswerTransport::Stdin);

        // Mid-stream injection is read from the dedicated top-level flag, so
        // the `capabilities` blob above could not switch it on.
        let midstream: ProviderRegistration = serde_json::from_value(serde_json::json!({
            "id": "ms",
            "display_name": "Ms",
            "models": [{ "id": "m1", "display_name": "M1" }],
            "supports_mid_stream_injection": true,
        }))
        .unwrap();
        assert!(effective_capabilities(&midstream).supports_mid_stream_injection);
    }

    #[test]
    fn completed_with_result_meta_error_is_a_failed_turn() {
        match terminal_from_completed(&serde_json::json!({})) {
            Terminal::Completed => {}
            other => panic!("empty meta should complete: {other:?}"),
        }
        match terminal_from_completed(&serde_json::json!({
            "error": "Failed to authenticate: OAuth session expired",
            "errorKind": "auth_expired",
        })) {
            Terminal::Crashed { kind, reason } => {
                assert_eq!(kind, CrashKind::AuthExpired);
                assert!(reason.contains("OAuth"));
            }
            Terminal::Completed => panic!("auth error must fail the turn"),
        }
        match terminal_from_completed(&serde_json::json!({
            "error": "Failed to authenticate. API Error: 401",
        })) {
            Terminal::Crashed { kind, .. } => assert_eq!(kind, CrashKind::AuthExpired),
            Terminal::Completed => panic!("401 text must classify as auth"),
        }
    }

    #[test]
    fn validate_registration_rejects_bad_ids_and_models() {
        for bad in ["", "Has-Upper", "with space", "semi;colon", &"x".repeat(65)] {
            assert!(validate_registration(&reg(bad)).is_err(), "id {bad:?}");
        }
        let mut r = reg("ok");
        r.models.clear();
        assert!(validate_registration(&r).is_err(), "empty models");
        let mut r = reg("ok");
        r.display_name = "  ".into();
        assert!(validate_registration(&r).is_err(), "blank display name");
        let mut r = reg("ok");
        r.models[0].id = "m@acct".into();
        assert!(validate_registration(&r).is_err(), "model id with @");
        let mut r = reg("ok");
        r.models.push(r.models[0].clone());
        assert!(validate_registration(&r).is_err(), "duplicate model id");
    }

    #[test]
    fn runtime_guards_turn_ownership_and_stop_flag() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let runtime = PluginProviderRuntime::new();
        let db = Db::in_memory().unwrap();
        runtime
            .begin_turn(
                "s1",
                TurnState {
                    plugin_id: "p1".into(),
                    stop: AtomicBool::new(false),
                    terminal: std::sync::Mutex::new(None),
                    injected: Default::default(),
                    stdin_q: Default::default(),
                    plugins: None,
                    db,
                    broadcaster: Broadcaster::new(),
                    rt: rt.handle().clone(),
                    snapshot: SessionSnapshot {
                        folder_path: "/tmp/x".into(),
                        folder_id: String::new(),
                        card_id: Some("c1".into()),
                        project_id: None,
                        is_worker: true,
                        mcp_config_path: Some("/tmp/mcp.json".into()),
                    },
                },
            )
            .unwrap();

        // Double-begin refused; ownership enforced for the foreign plugin.
        assert!(
            runtime
                .begin_turn(
                    "s1",
                    TurnState {
                        plugin_id: "p1".into(),
                        stop: AtomicBool::new(false),
                        terminal: std::sync::Mutex::new(None),
                        injected: Default::default(),
                        stdin_q: Default::default(),
                        plugins: None,
                        db: Db::in_memory().unwrap(),
                        broadcaster: Broadcaster::new(),
                        rt: rt.handle().clone(),
                        snapshot: SessionSnapshot {
                            folder_path: String::new(),
                            folder_id: String::new(),
                            card_id: None,
                            project_id: None,
                            is_worker: false,
                            mcp_config_path: None,
                        },
                    },
                )
                .is_err()
        );
        let foreign = runtime.emit_from_plugin(
            "p2",
            &serde_json::json!({ "session_id": "s1", "event": { "kind": "text", "text": "hi" } })
                .to_string(),
        );
        assert!(foreign.contains("another plugin"), "got: {foreign}");

        // Owned session snapshot round-trips; foreign/missing sessions refuse.
        let snap: serde_json::Value =
            serde_json::from_str(&runtime.get_session_json("p1", r#"{"session_id":"s1"}"#))
                .unwrap();
        assert_eq!(snap["folder_path"], "/tmp/x");
        assert_eq!(snap["card_id"], "c1");
        assert_eq!(snap["is_worker"], true);
        let mcp: serde_json::Value =
            serde_json::from_str(&runtime.get_mcp_config_json("p1", r#"{"session_id":"s1"}"#))
                .unwrap();
        assert_eq!(mcp["path"], "/tmp/mcp.json");
        assert!(
            runtime
                .get_session_json("p2", r#"{"session_id":"s1"}"#)
                .contains("error")
        );

        // Stop flag: false until requested; foreign/unknown polls read true.
        let owned: serde_json::Value =
            serde_json::from_str(&runtime.should_stop_json("p1", r#"{"session_id":"s1"}"#))
                .unwrap();
        assert_eq!(owned["stop"], false);
        runtime.request_stop("s1");
        let owned: serde_json::Value =
            serde_json::from_str(&runtime.should_stop_json("p1", r#"{"session_id":"s1"}"#))
                .unwrap();
        assert_eq!(owned["stop"], true);
        let unknown: serde_json::Value =
            serde_json::from_str(&runtime.should_stop_json("p1", r#"{"session_id":"nope"}"#))
                .unwrap();
        assert_eq!(unknown["stop"], true);

        // Mid-stream injection: only the owning plugin may enqueue, and the
        // turn drains its queue FIFO.
        assert!(!runtime.queue_injection("p2", "s1", serde_json::json!({ "text": "foreign" })));
        assert!(!runtime.queue_injection("p1", "nope", serde_json::json!({ "text": "gone" })));
        assert!(runtime.queue_injection("p1", "s1", serde_json::json!({ "text": "first" })));
        assert!(runtime.queue_injection("p1", "s1", serde_json::json!({ "text": "second" })));
        for expected in ["first", "second"] {
            let taken: serde_json::Value =
                serde_json::from_str(&runtime.take_message_json("p1", r#"{"session_id":"s1"}"#))
                    .unwrap();
            assert_eq!(taken["message"]["text"], expected);
        }
        let drained: serde_json::Value =
            serde_json::from_str(&runtime.take_message_json("p1", r#"{"session_id":"s1"}"#))
                .unwrap();
        assert!(drained["message"].is_null());
        assert!(
            runtime
                .take_message_json("p2", r#"{"session_id":"s1"}"#)
                .contains("error")
        );

        assert!(runtime.is_active("s1"));
        assert!(runtime.end_turn("s1").is_none());
        assert!(!runtime.is_active("s1"));
    }

    fn begin_test_turn(runtime: &PluginProviderRuntime, folder: &str) -> tokio::runtime::Runtime {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .begin_turn(
                "s1",
                TurnState {
                    plugin_id: "p1".into(),
                    stop: AtomicBool::new(false),
                    terminal: std::sync::Mutex::new(None),
                    injected: Default::default(),
                    stdin_q: Default::default(),
                    plugins: None,
                    db: Db::in_memory().unwrap(),
                    broadcaster: Broadcaster::new(),
                    rt: rt.handle().clone(),
                    snapshot: SessionSnapshot {
                        folder_path: folder.into(),
                        folder_id: String::new(),
                        card_id: None,
                        project_id: None,
                        is_worker: false,
                        mcp_config_path: None,
                    },
                },
            )
            .unwrap();
        rt
    }

    #[test]
    fn runtime_spawns_cli_child_reads_lines_and_eof() {
        let folder = std::env::temp_dir().to_string_lossy().into_owned();
        let runtime = PluginProviderRuntime::new();
        let _rt = begin_test_turn(&runtime, &folder);

        let spawn: serde_json::Value = serde_json::from_str(
            &runtime.spawn_json(
                "p1",
                &serde_json::json!({
                    "session_id": "s1",
                    "command": "/bin/echo",
                    "args": ["hello-plugin-cli"],
                    "cwd": folder,
                })
                .to_string(),
            ),
        )
        .unwrap();
        assert_eq!(spawn["ok"], true, "spawn: {spawn}");

        let line: serde_json::Value = serde_json::from_str(&runtime.read_line_json(
            "p1",
            &serde_json::json!({ "session_id": "s1", "timeout_ms": 2000 }).to_string(),
        ))
        .unwrap();
        assert_eq!(line["line"], "hello-plugin-cli", "line: {line}");

        let eof: serde_json::Value = serde_json::from_str(&runtime.read_line_json(
            "p1",
            &serde_json::json!({ "session_id": "s1", "timeout_ms": 2000 }).to_string(),
        ))
        .unwrap();
        assert_eq!(eof["eof"], true, "eof: {eof}");
        assert_eq!(eof["exit_code"], 0);

        // Foreign plugin cannot spawn on this turn.
        let runtime2 = PluginProviderRuntime::new();
        let _rt2 = begin_test_turn(&runtime2, &folder);
        let foreign = runtime2.spawn_json(
            "p2",
            &serde_json::json!({
                "session_id": "s1",
                "command": "/bin/echo",
                "args": ["nope"],
            })
            .to_string(),
        );
        assert!(
            foreign.contains("another plugin") || foreign.contains("error"),
            "got: {foreign}"
        );

        runtime.end_turn("s1");
    }

    #[test]
    fn runtime_write_stdin_round_trips_through_cat() {
        let folder = std::env::temp_dir().to_string_lossy().into_owned();
        let runtime = PluginProviderRuntime::new();
        let _rt = begin_test_turn(&runtime, &folder);

        let spawn: serde_json::Value = serde_json::from_str(
            &runtime.spawn_json(
                "p1",
                &serde_json::json!({
                    "session_id": "s1",
                    "command": "/bin/cat",
                    "cwd": folder,
                })
                .to_string(),
            ),
        )
        .unwrap();
        assert_eq!(spawn["ok"], true, "spawn: {spawn}");

        assert!(runtime.write_child_stdin("s1", "via-adapter\n"));
        let via_plugin = runtime.write_stdin_json(
            "p1",
            &serde_json::json!({ "session_id": "s1", "text": "via-plugin\n" }).to_string(),
        );
        assert!(via_plugin.contains("ok"), "write: {via_plugin}");

        let first: serde_json::Value = serde_json::from_str(&runtime.read_line_json(
            "p1",
            &serde_json::json!({ "session_id": "s1", "timeout_ms": 2000 }).to_string(),
        ))
        .unwrap();
        let second: serde_json::Value = serde_json::from_str(&runtime.read_line_json(
            "p1",
            &serde_json::json!({ "session_id": "s1", "timeout_ms": 2000 }).to_string(),
        ))
        .unwrap();
        let got = [
            first["line"].as_str().unwrap_or(""),
            second["line"].as_str().unwrap_or(""),
        ];
        assert!(got.contains(&"via-adapter"), "got {got:?}");
        assert!(got.contains(&"via-plugin"), "got {got:?}");

        runtime.kill_json("p1", r#"{"session_id":"s1"}"#);
        runtime.end_turn("s1");
    }

    #[test]
    fn runtime_spawn_rejects_cwd_outside_session_folder() {
        let folder = std::env::temp_dir().to_string_lossy().into_owned();
        let runtime = PluginProviderRuntime::new();
        let _rt = begin_test_turn(&runtime, &folder);
        let err = runtime.spawn_json(
            "p1",
            &serde_json::json!({
                "session_id": "s1",
                "command": "/bin/echo",
                "cwd": "/etc",
            })
            .to_string(),
        );
        assert!(err.contains("cwd must equal"), "got: {err}");
        runtime.end_turn("s1");
    }

    #[test]
    fn runtime_invoke_mcp_refuses_without_plugin_manager() {
        let folder = std::env::temp_dir().to_string_lossy().into_owned();
        let runtime = PluginProviderRuntime::new();
        let _rt = begin_test_turn(&runtime, &folder);
        let out = runtime.invoke_mcp_json(
            "p1",
            r#"{"session_id":"s1","name":"list_models","arguments":{}}"#,
        );
        assert!(out.contains("no plugin manager"), "got: {out}");
        runtime.end_turn("s1");
    }
}
