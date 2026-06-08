use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{Mutex, Notify, mpsc};

use crate::provider::agent::{AgentProvider, ProcessCompletion, SendMessageContext};
use crate::provider::registry::{ProviderInfo, ProviderRegistry};

use super::process::{self, LoopState, StdinMsg};

/// Per-session tracking entry for a running Claude CLI invocation.
///
/// The Claude CLI is spawned ONCE per session and persists across
/// turns; the streaming task owns the `Child`, this struct just
/// carries the handles needed to talk to it. To stop the child we
/// notify `cancel`; the stream loop calls `start_kill` and synthesises
/// a Crashed event so the orchestrator sees a normal completion.
struct ClaudeRun {
    cancel: Arc<Notify>,
    stdin_tx: mpsc::Sender<StdinMsg>,
    /// True while a user turn is in flight (between `send_message`
    /// writing a user envelope and the CLI emitting `result`). Read
    /// from outside the loop by `is_running` and the idle reaper.
    turn_active: Arc<AtomicBool>,
    /// Epoch ms of the last activity (event from the CLI or write to
    /// stdin). Used by the idle reaper to decide whether to recycle
    /// a quiet child.
    last_activity: Arc<AtomicU64>,
}

/// `AgentProvider` impl backed by the Claude CLI in stream-json
/// duplex mode.
///
/// Owns one long-lived child process per session. The first
/// `send_message` for a session spawns it and writes the initial
/// user envelope to stdin; subsequent messages — including those
/// that arrive while a turn is still in flight — write straight to
/// stdin and the CLI consumes them after the current turn finishes.
/// That's the mid-stream injection contract: there is no
/// peckboard-level queue, the CLI itself is the queue.
///
/// The dispatcher delegates here once it has resolved the model
/// prefix to `"claude"`.
pub struct ClaudeProvider {
    runs: Arc<Mutex<HashMap<String, ClaudeRun>>>,
}

impl ClaudeProvider {
    pub fn new() -> Self {
        ClaudeProvider {
            runs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Start the idle-process reaper as a background task.
    ///
    /// Walks the run map every `tick` and kills any child that has
    /// no turn in flight AND has been silent for at least `idle_ms`.
    /// Killing notifies the run's cancel; the stream loop tears
    /// the child down, removes its row from the map, and the next
    /// `send_message` respawns with `--resume <conv_id>` so the
    /// conversation continues seamlessly.
    ///
    /// Without the reaper, a panel with N sessions left open
    /// overnight would keep N `claude` subprocesses alive forever.
    pub fn spawn_idle_reaper(self: &Arc<Self>, idle_ms: u64, tick: Duration) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(tick);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let Some(provider) = weak.upgrade() else {
                    break;
                };
                provider.sweep_idle(idle_ms).await;
            }
        });
    }

    async fn sweep_idle(&self, idle_ms: u64) {
        let now = now_ms();
        let to_kill: Vec<(String, Arc<Notify>)> = {
            let runs = self.runs.lock().await;
            runs.iter()
                .filter(|(_, r)| {
                    !r.turn_active.load(Ordering::Acquire)
                        && now.saturating_sub(r.last_activity.load(Ordering::Acquire)) >= idle_ms
                })
                .map(|(sid, r)| (sid.clone(), r.cancel.clone()))
                .collect()
        };
        for (sid, cancel) in to_kill {
            tracing::info!(session_id = %sid, "Idle reaper killing stale claude run");
            cancel.notify_one();
        }
    }
}

impl Default for ClaudeProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentProvider for ClaudeProvider {
    fn id(&self) -> &str {
        "claude"
    }

    async fn send_message(&self, ctx: SendMessageContext) -> anyhow::Result<()> {
        let SendMessageContext {
            session_id,
            message,
            db,
            broadcaster,
            config,
            conversation_id,
            completion_tx,
            // Claude parses its own TodoWrite calls into `todo` events
            // (see process.rs); it has no need for the plugin todo path.
            plugins: _,
        } = ctx;

        // Strip the `claude:` prefix if present so the CLI sees the bare
        // model id (e.g. `claude-opus-4-7`).
        let cli_config = crate::provider::stream::SpawnConfig {
            model: config
                .model
                .strip_prefix("claude:")
                .map(|m| m.to_string())
                .unwrap_or(config.model.clone()),
            ..config
        };

        // Lock the runs map ONCE, then either reuse the existing run's
        // stdin or spawn a new child and insert. The lock spans the
        // is-present check + insert so two concurrent first-sends for
        // the same session can't both spawn.
        let stdin_tx = {
            let mut runs = self.runs.lock().await;

            if let Some(existing) = runs.get(&session_id) {
                existing.stdin_tx.clone()
            } else {
                let process =
                    process::spawn_claude(&session_id, &cli_config, conversation_id.as_deref())?;

                let (tx, rx) = mpsc::channel::<StdinMsg>(64);
                let cancel = Arc::new(Notify::new());
                let turn_active = Arc::new(AtomicBool::new(false));
                let last_activity = Arc::new(AtomicU64::new(now_ms()));

                let run = ClaudeRun {
                    cancel: cancel.clone(),
                    stdin_tx: tx.clone(),
                    turn_active: turn_active.clone(),
                    last_activity: last_activity.clone(),
                };
                runs.insert(session_id.clone(), run);

                let allowed_dir = cli_config.working_dir.clone();
                let runs_arc = self.runs.clone();
                let sid = session_id.clone();
                let completion_tx_clone = completion_tx.clone();
                let state = LoopState {
                    turn_active,
                    last_activity,
                };
                tokio::spawn(async move {
                    let is_completed = process::stream_events(
                        process,
                        db,
                        broadcaster,
                        rx,
                        allowed_dir,
                        cancel,
                        state,
                    )
                    .await;

                    runs_arc.lock().await.remove(&sid);

                    tracing::debug!(
                        session_id = %sid,
                        "Claude stream task finished, run removed from manager"
                    );

                    let _ = completion_tx_clone
                        .send(ProcessCompletion {
                            session_id: sid,
                            completed: is_completed,
                        })
                        .await;
                });

                tx
            }
        };

        // Dispatch the user turn. Errors here mean the stream task
        // has already shut down (channel closed) — return the error
        // so the caller can append a Crashed and let the user retry.
        stdin_tx
            .send(StdinMsg::UserTurn(message))
            .await
            .map_err(|e| anyhow::anyhow!("stdin channel closed: {e}"))?;

        Ok(())
    }

    async fn cancel(&self, session_id: &str) {
        let cancel = {
            let runs = self.runs.lock().await;
            runs.get(session_id).map(|r| r.cancel.clone())
        };
        match cancel {
            Some(c) => {
                tracing::info!(session_id = %session_id, "Cancelling claude run");
                c.notify_one();
            }
            None => {
                tracing::debug!(
                    session_id = %session_id,
                    "No tracked claude run to cancel (may have already exited)"
                );
            }
        }
    }

    async fn interrupt(&self, session_id: &str) {
        // Claude CLI in stream-json mode does not respond to in-band
        // interrupt bytes for a hard stop — the only reliable way to
        // stop a wedged process is to kill it. Reuse the cancel path
        // so the stream loop exits and a completion notification is
        // delivered to the orchestrator.
        self.cancel(session_id).await;
    }

    async fn write_stdin(&self, session_id: &str, text: &str) -> bool {
        let tx = {
            let runs = self.runs.lock().await;
            runs.get(session_id).map(|r| r.stdin_tx.clone())
        };
        let Some(tx) = tx else {
            tracing::debug!(
                session_id = %session_id,
                "No stdin channel for claude session (process may have exited)"
            );
            return false;
        };
        match tx.try_send(StdinMsg::RawLine(text.to_string())) {
            Ok(()) => {
                tracing::info!(
                    session_id = %session_id,
                    "Sent raw stdin message to claude process"
                );
                true
            }
            Err(e) => {
                tracing::warn!(
                    session_id = %session_id,
                    "Failed to send stdin message: {e}"
                );
                false
            }
        }
    }

    async fn is_running(&self, session_id: &str) -> bool {
        let runs = self.runs.lock().await;
        runs.get(session_id)
            .map(|r| r.turn_active.load(Ordering::Acquire))
            .unwrap_or(false)
    }

    fn supports_mid_stream_injection(&self) -> bool {
        // The CLI in stream-json mode reads user envelopes from
        // stdin at any time and consumes them after the current
        // `result`. Concurrent dispatches share the same long-
        // lived child.
        true
    }

    async fn cleanup(&self) {
        // The stream task removes itself from the map on completion,
        // so there is nothing to sweep here. Kept as a no-op for API
        // parity.
    }

    async fn shutdown(&self) {
        let entries: Vec<(String, Arc<Notify>)> = {
            let runs = self.runs.lock().await;
            runs.iter()
                .map(|(sid, r)| (sid.clone(), r.cancel.clone()))
                .collect()
        };
        if entries.is_empty() {
            return;
        }

        tracing::info!("Shutting down {} running claude run(s)", entries.len());
        for (session_id, cancel) in entries {
            tracing::info!(session_id = %session_id, "Notifying claude run to shut down");
            cancel.notify_one();
        }
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Register the Claude CLI provider in the registry and start the
/// idle-process reaper.
///
/// `idle_ms` defaults to 30 minutes — gives the user a meaningful
/// window of "I came back to my tab after a meeting" without the
/// previous turn paying the spawn cost again. Set very low in tests
/// so they don't have to wait.
pub async fn register_claude_provider(registry: &ProviderRegistry) {
    let provider = Arc::new(ClaudeProvider::new());
    let models = super::discover_models();

    provider.spawn_idle_reaper(30 * 60 * 1_000, Duration::from_secs(60));

    registry
        .register(
            provider,
            ProviderInfo {
                id: "claude".into(),
                display_name: "Claude (CLI)".into(),
                models,
            },
        )
        .await;
}
