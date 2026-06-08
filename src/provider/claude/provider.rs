use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{Mutex, Notify};

use crate::provider::agent::{AgentProvider, ProcessCompletion, SendMessageContext};
use crate::provider::registry::{ProviderInfo, ProviderRegistry};

use super::process;

/// Per-session tracking entry for a running Claude CLI invocation.
///
/// We deliberately do NOT hold the `Child` here — the streaming task owns
/// it exclusively, which keeps stdin/stdout/wait access lock-free. To stop
/// the child we notify `cancel`; the stream loop calls `start_kill` and
/// then emits a Crashed event so the orchestrator sees a normal completion.
struct ClaudeRun {
    cancel: Arc<Notify>,
}

/// `AgentProvider` impl backed by the Claude CLI (`claude -p ...`).
///
/// Owns the per-session run map and stdin channels that used to live
/// in `SessionManager`. The dispatcher delegates here once it has resolved
/// the model prefix to `"claude"`.
pub struct ClaudeProvider {
    runs: Arc<Mutex<HashMap<String, ClaudeRun>>>,
    stdin_channels: Arc<Mutex<HashMap<String, tokio::sync::mpsc::Sender<String>>>>,
}

impl ClaudeProvider {
    pub fn new() -> Self {
        ClaudeProvider {
            runs: Arc::new(Mutex::new(HashMap::new())),
            stdin_channels: Arc::new(Mutex::new(HashMap::new())),
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

        // Cancel any prior run before spawning a fresh one. We notify the
        // old run and let its stream task clean up; the new run will
        // register itself below.
        {
            let mut runs = self.runs.lock().await;
            if let Some(old) = runs.remove(&session_id) {
                tracing::warn!(
                    session_id = %session_id,
                    "Cancelling previous claude run before spawning new one"
                );
                old.cancel.notify_one();
            }
        }

        let child = process::spawn_claude(
            &session_id,
            &message,
            &cli_config,
            conversation_id.as_deref(),
        )?;

        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::channel::<String>(32);
        let stdin_tx_for_stream = stdin_tx.clone();
        let allowed_dir = cli_config.working_dir.clone();
        {
            let mut channels = self.stdin_channels.lock().await;
            channels.insert(session_id.clone(), stdin_tx);
        }

        let cancel = Arc::new(Notify::new());
        {
            let mut runs = self.runs.lock().await;
            runs.insert(
                session_id.clone(),
                ClaudeRun {
                    cancel: cancel.clone(),
                },
            );
        }

        let runs = self.runs.clone();
        let stdin_channels = self.stdin_channels.clone();
        let sid = session_id.clone();
        tokio::spawn(async move {
            let is_completed = process::stream_events(
                child,
                db,
                broadcaster,
                stdin_rx,
                stdin_tx_for_stream,
                allowed_dir,
                cancel,
            )
            .await;

            {
                let mut map = runs.lock().await;
                map.remove(&sid);
            }
            {
                let mut channels = stdin_channels.lock().await;
                channels.remove(&sid);
            }

            tracing::debug!(
                session_id = %sid,
                "Claude process streaming completed, removed from manager"
            );

            let _ = completion_tx
                .send(ProcessCompletion {
                    session_id: sid,
                    completed: is_completed,
                })
                .await;
        });

        Ok(())
    }

    async fn cancel(&self, session_id: &str) {
        let removed = {
            let mut map = self.runs.lock().await;
            map.remove(session_id)
        };
        match removed {
            Some(run) => {
                tracing::info!(session_id = %session_id, "Cancelling claude run");
                run.cancel.notify_one();
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
        // interrupt bytes — the only reliable way to stop it is to kill
        // the process. Reuse the cancel path so the stream loop exits and
        // a completion notification is delivered to the orchestrator.
        self.cancel(session_id).await;
    }

    async fn write_stdin(&self, session_id: &str, text: &str) -> bool {
        let channels = self.stdin_channels.lock().await;
        if let Some(tx) = channels.get(session_id) {
            match tx.try_send(text.to_string()) {
                Ok(()) => {
                    tracing::info!(
                        session_id = %session_id,
                        "Sent stdin message to claude process"
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
        } else {
            tracing::debug!(
                session_id = %session_id,
                "No stdin channel for claude session (process may have exited)"
            );
            false
        }
    }

    async fn is_running(&self, session_id: &str) -> bool {
        let map = self.runs.lock().await;
        map.contains_key(session_id)
    }

    async fn cleanup(&self) {
        // The stream task removes itself from the map on completion, so
        // there is nothing to sweep here. Kept as a no-op for API parity.
    }

    async fn shutdown(&self) {
        let entries: Vec<(String, ClaudeRun)> = {
            let mut map = self.runs.lock().await;
            map.drain().collect()
        };
        if entries.is_empty() {
            return;
        }

        tracing::info!("Shutting down {} running claude run(s)", entries.len());
        for (session_id, run) in entries {
            tracing::info!(session_id = %session_id, "Notifying claude run to shut down");
            run.cancel.notify_one();
        }
    }
}

/// Register the Claude CLI provider in the registry.
pub async fn register_claude_provider(registry: &ProviderRegistry) {
    let provider = Arc::new(ClaudeProvider::new());
    let models = super::discover_models();

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
