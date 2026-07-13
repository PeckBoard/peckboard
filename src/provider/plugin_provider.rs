//! WASM-plugin-backed AI providers.
//!
//! A plugin that declares the `provider.register` hook (plus the
//! `register_provider` permission and the `provider.send` hook) can register
//! an AI provider that behaves like any native one: its models show up in
//! `/api/models` and the MCP `list_models` tool, and sessions dispatch to it
//! through the ordinary `ProviderRegistry` lookup — the `SessionManager`
//! needs zero special-casing.
//!
//! The bridge is [`PluginProviderAdapter`], an [`AgentProvider`] whose
//! `send_message` runs one **turn per WASM call**: it dispatches the
//! `provider.send` hook to the owning plugin on a dedicated blocking thread
//! (via [`crate::plugin::manager::PluginManager::dispatch_provider_send`])
//! and, while that call is in flight, the plugin streams
//! [`ProviderEvent`]s back through the `peckboard_emit_provider_event` host
//! function. Those events feed the shared [`emit_event`] path, so DB append,
//! `usage_events` rows, `conversation_id` persistence, and the WS broadcast
//! all work unchanged.
//!
//! v1 scope: HTTP-API providers only (OpenAI-compatible request/response or
//! chunked HTTP the plugin consumes inside the call). No subprocess CLIs, no
//! host-side SSE plumbing. Interrupts are cooperative — the adapter sets a
//! per-session stop flag the plugin polls via `peckboard_provider_should_stop`
//! between chunks; the per-call WASM timeout guarantees termination either way.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use serde::Deserialize;

use crate::db::Db;
use crate::plugin::manager::PluginManager;
use crate::provider::agent::{AgentProvider, ProcessCompletion, SendMessageContext, emit_event};
use crate::provider::registry::EffortLevel;
use crate::provider::stream::{ModelInfo, ProviderEvent};
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
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ModelPricing {
    pub input_usd_per_mtok: f64,
    pub output_usd_per_mtok: f64,
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
    if reg.models.is_empty() {
        return Err("a provider must register at least one model".into());
    }
    let mut seen = std::collections::HashSet::new();
    for m in &reg.models {
        // `@` would break the account-suffix split and whitespace breaks
        // everything downstream; `:` is tolerated (model-id parsing splits
        // on the FIRST colon, which the provider prefix owns).
        if m.id.is_empty() || m.id.contains('@') || m.id.chars().any(char::is_whitespace) {
            return Err(format!(
                "model id '{}' is invalid: must be non-empty, no '@', no whitespace",
                m.id
            ));
        }
        if !seen.insert(m.id.as_str()) {
            return Err(format!("duplicate model id '{}'", m.id));
        }
    }
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
    Crashed { reason: String },
}

/// Trusted, host-side snapshot of the session a provider turn is running
/// for. Captured by the adapter at dispatch time from the resolved
/// `SendMessageContext` (never from plugin-supplied ids) and served back to
/// the plugin by `peckboard_provider_get_session` / `_get_mcp_config`.
#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub folder_path: String,
    pub card_id: Option<String>,
    pub project_id: Option<String>,
    pub is_worker: bool,
    pub mcp_config_path: Option<String>,
}

/// One in-flight `provider.send` turn.
struct TurnState {
    /// The plugin executing this turn — the ONLY plugin allowed to emit
    /// events into the session while the turn is active.
    plugin_id: String,
    stop: AtomicBool,
    terminal: std::sync::Mutex<Option<Terminal>>,
    db: Db,
    broadcaster: Arc<Broadcaster>,
    /// Runtime handle for `block_on` from the host function. Safe because a
    /// turn's host calls only ever run on the dedicated `spawn_blocking`
    /// thread driving the plugin's `provider.send` call (the per-plugin
    /// mutex serialises all other dispatches to that plugin for the whole
    /// turn), never on an async worker thread.
    rt: tokio::runtime::Handle,
    snapshot: SessionSnapshot,
}

/// Host-side state shared between every [`PluginProviderAdapter`] and the
/// provider host functions in `src/plugin/host.rs`: which session has a turn
/// in flight, owned by which plugin, plus the stop flag and the trusted
/// session snapshot. One instance per [`PluginManager`].
#[derive(Default)]
pub struct PluginProviderRuntime {
    turns: std::sync::Mutex<HashMap<String, Arc<TurnState>>>,
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

    fn begin_turn(&self, session_id: &str, turn: TurnState) -> Result<(), String> {
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
    fn end_turn(&self, session_id: &str) -> Option<Terminal> {
        let turn = self.turns.lock().ok()?.remove(session_id)?;
        let terminal = turn.terminal.lock().ok()?.clone();
        terminal
    }

    pub fn is_active(&self, session_id: &str) -> bool {
        self.turn(session_id).is_some()
    }

    /// Cooperative interrupt: flag the turn so the plugin's next
    /// `peckboard_provider_should_stop` poll returns true.
    pub fn request_stop(&self, session_id: &str) {
        if let Some(turn) = self.turn(session_id) {
            turn.stop.store(true, Ordering::SeqCst);
        }
    }

    /// Flag every in-flight turn owned by `plugin_id` — used when the plugin
    /// is unloaded/denied so orphaned turns wind down at their next poll.
    pub fn request_stop_for_plugin(&self, plugin_id: &str) {
        if let Ok(turns) = self.turns.lock() {
            for turn in turns.values() {
                if turn.plugin_id == plugin_id {
                    turn.stop.store(true, Ordering::SeqCst);
                }
            }
        }
    }

    // ── Host-function backends (JSON-string in/out, never panic) ──────

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
                ProviderEvent::Completed { .. } => *terminal = Some(Terminal::Completed),
                ProviderEvent::Crashed { reason, .. } => {
                    *terminal = Some(Terminal::Crashed {
                        reason: reason.clone(),
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
    pub fn get_session_json(&self, plugin_id: &str, input: &str) -> String {
        let req: SessionIdRequest = match serde_json::from_str(input) {
            Ok(r) => r,
            Err(e) => return error_json(format!("invalid request: {e}")),
        };
        match self.owned_turn(plugin_id, &req.session_id) {
            Ok(turn) => serde_json::json!({
                "session_id": req.session_id,
                "folder_path": turn.snapshot.folder_path,
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
            Ok(turn) => serde_json::json!({ "path": turn.snapshot.mcp_config_path }).to_string(),
            Err(e) => error_json(e),
        }
    }
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

    async fn send_message(&self, ctx: SendMessageContext) -> anyhow::Result<()> {
        let session = ctx
            .db
            .get_session(&ctx.session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("session not found: {}", ctx.session_id))?;
        let snapshot = SessionSnapshot {
            folder_path: ctx.config.working_dir.clone(),
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
                },
            )
            .map_err(|e| anyhow::anyhow!(e))?;

        use base64::Engine as _;
        let attachments: Vec<serde_json::Value> = ctx
            .message
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
        let payload = serde_json::json!({
            "session_id": ctx.session_id,
            "provider_id": self.provider_id,
            "spawn_config": ctx.config,
            "message": { "text": ctx.message.text, "attachments": attachments },
            "conversation_id": ctx.conversation_id,
        });

        let manager = self.manager.clone();
        let runtime = self.runtime.clone();
        let plugin_id = self.plugin_id.clone();
        let session_id = ctx.session_id.clone();
        let db = ctx.db.clone();
        let broadcaster = ctx.broadcaster.clone();
        let completion_tx = ctx.completion_tx.clone();
        tokio::spawn(async move {
            let result = manager.dispatch_provider_send(&plugin_id, payload).await;
            let terminal = runtime.end_turn(&session_id);
            let (completed, error) = match terminal {
                // The plugin already reported how the turn ended; a trap
                // AFTER a terminal event doesn't retroactively fail it.
                Some(Terminal::Completed) => (true, None),
                Some(Terminal::Crashed { reason }) => (false, Some(reason)),
                None => {
                    let reason = match result {
                        Err(e) => e,
                        Ok(()) => "plugin provider returned without emitting Completed or Crashed"
                            .to_string(),
                    };
                    emit_event(
                        &db,
                        &broadcaster,
                        &session_id,
                        ProviderEvent::Crashed {
                            reason: reason.clone(),
                            exit_code: None,
                            stderr: None,
                        },
                    )
                    .await;
                    (false, Some(reason))
                }
            };
            let _ = completion_tx
                .send(ProcessCompletion {
                    session_id,
                    completed,
                    error,
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
        // backstop that guarantees the turn terminates regardless.
        self.runtime.request_stop(session_id);
    }

    async fn write_stdin(&self, _session_id: &str, _text: &str) -> bool {
        false
    }

    async fn is_running(&self, session_id: &str) -> bool {
        self.runtime.is_active(session_id)
    }

    async fn wait_for_termination(&self, session_id: &str) {
        while self.runtime.is_active(session_id) {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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
                    db,
                    broadcaster: Broadcaster::new(),
                    rt: rt.handle().clone(),
                    snapshot: SessionSnapshot {
                        folder_path: "/tmp/x".into(),
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
                        db: Db::in_memory().unwrap(),
                        broadcaster: Broadcaster::new(),
                        rt: rt.handle().clone(),
                        snapshot: SessionSnapshot {
                            folder_path: String::new(),
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

        assert!(runtime.is_active("s1"));
        assert!(runtime.end_turn("s1").is_none());
        assert!(!runtime.is_active("s1"));
    }
}
