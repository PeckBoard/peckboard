//! Capability executors: the code that actually runs a requested action.
//!
//! Each capability is a [`CapabilityExecutor`]. Real executors ship for
//! `echo`, `terminal`, `server`, and `screenshot`; the remaining OS
//! capabilities (mouse / keyboard) stay [`StubExecutor`]s until their own
//! cards land.
//!
//! Gating is NOT done here — [`Executors::dispatch`] runs the executor
//! unconditionally. The caller ([`crate::client`]) checks `Config::is_enabled`
//! first so the kill-switch and per-capability flags are enforced (and
//! audited) before we ever reach an executor.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use peckboard_agent_protocol::AgentFrame;

use crate::config::CAPABILITIES;
use crate::config::Config;
use crate::input::{ConsoleIndicator, EnigoBackend, Indicator, InputBackend};
use crate::input_exec::{KeyboardExecutor, MouseExecutor};
use crate::server_mgmt::ServerExecutor;
use crate::terminal::TerminalExecutor;

/// Per-request context handed to an executor. Carries the correlation id
/// (so streamed events can be tied back to the originating request), an
/// outbound sender for unsolicited [`AgentFrame::Event`] frames, and a
/// cancellation token the client trips when the server sends
/// [`peckboard_agent_protocol::ServerFrame::Cancel`].
#[derive(Clone)]
pub struct ExecContext {
    pub corr_id: String,
    pub events: mpsc::Sender<AgentFrame>,
    pub cancel: CancellationToken,
}

impl ExecContext {
    /// Emit an unsolicited event frame; best-effort (dropped if the
    /// outbound buffer is full or the writer is gone).
    pub async fn emit(&self, kind: &str, data: Value) {
        let _ = self
            .events
            .send(AgentFrame::Event {
                kind: kind.to_string(),
                data,
            })
            .await;
    }
}

/// One remote-controllable capability. Async so real executors (e.g.
/// terminal, screenshot) can await I/O without blocking the client loop.
#[async_trait]
pub trait CapabilityExecutor: Send + Sync {
    /// Run the capability with the request's `args`, returning a JSON
    /// payload on success or a human-readable error string on failure.
    /// `ctx` carries the correlation id, an event sender for streaming,
    /// and a cancellation token.
    async fn execute(&self, ctx: &ExecContext, args: Value) -> Result<Value, String>;
}

/// Echoes the request args straight back. Pairs with the Phase 3 echo loop
/// to prove the full request/response round-trip end to end.
pub struct EchoExecutor;

#[async_trait]
impl CapabilityExecutor for EchoExecutor {
    async fn execute(&self, _ctx: &ExecContext, args: Value) -> Result<Value, String> {
        Ok(args)
    }
}

/// Placeholder for a capability whose real executor hasn't been built yet.
pub struct StubExecutor {
    name: &'static str,
}

#[async_trait]
impl CapabilityExecutor for StubExecutor {
    async fn execute(&self, _ctx: &ExecContext, _args: Value) -> Result<Value, String> {
        Err(format!("capability '{}' not implemented", self.name))
    }
}

/// The daemon's capability registry.
pub struct Executors {
    map: HashMap<String, Box<dyn CapabilityExecutor>>,
}

impl Executors {
    /// Registry with real `echo`/`terminal`/`server` executors and a stub
    /// for every other known capability, so an enabled-but-unbuilt
    /// capability reports "not implemented" rather than "unknown".
    ///
    /// `server` shares one [`ServerExecutor`] instance across dispatches so
    /// managed-process state (which servers this daemon has started, their
    /// log rings) survives between requests.
    pub fn with_defaults(config: Arc<Config>) -> Self {
        let mut map: HashMap<String, Box<dyn CapabilityExecutor>> = HashMap::new();
        map.insert("echo".to_string(), Box::new(EchoExecutor));
        map.insert("terminal".to_string(), Box::new(TerminalExecutor));
        map.insert("server".to_string(), Box::new(ServerExecutor::new(config)));
        map.insert(
            "screenshot".to_string(),
            Box::new(crate::screenshot::ScreenshotExecutor),
        );
        // Highest-risk capabilities: real OS input synthesis, behind a
        // shared enigo backend and a visible "controlling this machine"
        // indicator. OFF-by-default + the kill-switch are enforced upstream
        // in config.rs/client.rs before we're reached.
        let input_backend: Arc<dyn InputBackend> = Arc::new(EnigoBackend::new());
        let indicator: Arc<dyn Indicator> = Arc::new(ConsoleIndicator::new());
        // Live config path for the per-action kill-switch re-check inside the
        // input executors (see input_exec::run_action).
        let input_config_path = Config::config_path().ok();
        map.insert(
            "mouse".to_string(),
            Box::new(MouseExecutor::new(
                input_backend.clone(),
                indicator.clone(),
                input_config_path.clone(),
            )),
        );
        map.insert(
            "keyboard".to_string(),
            Box::new(KeyboardExecutor::new(
                input_backend,
                indicator,
                input_config_path,
            )),
        );
        for &name in CAPABILITIES {
            if map.contains_key(name) {
                continue;
            }
            map.insert(name.to_string(), Box::new(StubExecutor { name }));
        }
        Self { map }
    }

    /// Run a capability by name. Unknown names error rather than panic.
    pub async fn dispatch(
        &self,
        ctx: &ExecContext,
        capability: &str,
        args: Value,
    ) -> Result<Value, String> {
        match self.map.get(capability) {
            Some(executor) => executor.execute(ctx, args).await,
            None => Err(format!("unknown capability '{capability}'")),
        }
    }
}

/// Test-only helpers shared across the crate's executor tests.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;

    /// A context whose emitted events are collected into the returned
    /// receiver. Drain it after `execute` returns (all `emit` sends have
    /// completed by then) to assert on streamed frames.
    pub fn collecting_ctx() -> (ExecContext, mpsc::Receiver<AgentFrame>) {
        let (tx, rx) = mpsc::channel(1024);
        (
            ExecContext {
                corr_id: "test".to_string(),
                events: tx,
                cancel: CancellationToken::new(),
            },
            rx,
        )
    }

    /// A throwaway context whose events go nowhere — for tests that don't
    /// assert on streamed frames.
    pub fn test_ctx() -> ExecContext {
        collecting_ctx().0
    }

    /// Non-blockingly drain every buffered event out of a receiver.
    pub fn drain(rx: &mut mpsc::Receiver<AgentFrame>) -> Vec<AgentFrame> {
        let mut out = Vec::new();
        while let Ok(f) = rx.try_recv() {
            out.push(f);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::tests_support::test_ctx;
    use serde_json::json;

    fn execs() -> Executors {
        Executors::with_defaults(Arc::new(Config::default()))
    }

    #[tokio::test]
    async fn echo_returns_payload_unchanged() {
        let args = json!({"msg": "hi", "n": 3});
        let out = execs()
            .dispatch(&test_ctx(), "echo", args.clone())
            .await
            .unwrap();
        assert_eq!(out, args);
    }

    #[tokio::test]
    async fn stub_capabilities_report_not_implemented() {
        // No capability is a stub today; exercise StubExecutor directly so
        // the "enabled-but-unbuilt" fallback stays covered.
        let stub = StubExecutor { name: "future" };
        let err = stub.execute(&test_ctx(), json!({})).await.unwrap_err();
        assert!(err.contains("not implemented"), "got: {err}");
    }
    #[tokio::test]
    async fn unknown_capability_errors() {
        let err = execs()
            .dispatch(&test_ctx(), "nope", json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("unknown capability"), "got: {err}");
    }
}
