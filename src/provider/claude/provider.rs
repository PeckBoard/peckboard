use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::provider::agent::{AgentProvider, ProcessCompletion, SendMessageContext};
use crate::provider::registry::{ProviderInfo, ProviderRegistry};

use super::process::{self, ClaudeProcess};

/// `AgentProvider` impl backed by the Claude CLI (`claude -p ...`).
///
/// Owns the per-session process map and stdin channels that used to live
/// in `SessionManager`. The dispatcher delegates here once it has resolved
/// the model prefix to `"claude"`.
pub struct ClaudeProvider {
    processes: Arc<Mutex<HashMap<String, ClaudeProcess>>>,
    stdin_channels: Arc<Mutex<HashMap<String, tokio::sync::mpsc::Sender<String>>>>,
}

impl ClaudeProvider {
    pub fn new() -> Self {
        ClaudeProvider {
            processes: Arc::new(Mutex::new(HashMap::new())),
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

        let child = process::spawn_claude(
            &session_id,
            &message,
            &cli_config,
            conversation_id.as_deref(),
        )?;

        // If there's already a process for this session, kill it first.
        {
            let mut map = self.processes.lock().await;
            if let Some(old) = map.remove(&session_id) {
                tracing::warn!(
                    session_id = %session_id,
                    "Killing existing claude process before spawning new one"
                );
                tokio::spawn(async move {
                    process::kill_process(old).await;
                });
            }
        }

        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::channel::<String>(32);
        let stdin_tx_for_stream = stdin_tx.clone();
        let allowed_dir = cli_config.working_dir.clone();
        {
            let mut channels = self.stdin_channels.lock().await;
            channels.insert(session_id.clone(), stdin_tx);
        }

        let processes = self.processes.clone();
        let stdin_channels = self.stdin_channels.clone();
        let sid = session_id.clone();
        tokio::spawn(async move {
            let is_completed =
                process::stream_events(child, db, broadcaster, stdin_rx, stdin_tx_for_stream, allowed_dir).await;

            {
                let mut map = processes.lock().await;
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
        let mut map = self.processes.lock().await;
        if let Some(proc) = map.remove(session_id) {
            tracing::info!(session_id = %session_id, "Cancelling claude process");
            tokio::spawn(async move {
                process::kill_process(proc).await;
            });
        } else {
            tracing::debug!(
                session_id = %session_id,
                "No tracked claude process to cancel (may have already exited)"
            );
        }
    }

    async fn interrupt(&self, session_id: &str) {
        // Prefer the stdin channel (covers the common case where
        // stream_events owns the child).
        if self.write_stdin(session_id, "").await {
            tracing::info!(session_id = %session_id, "Sent interrupt via stdin channel");
            return;
        }

        let mut map = self.processes.lock().await;
        if let Some(proc) = map.get_mut(session_id) {
            process::interrupt_process(proc).await;
        } else {
            tracing::debug!(
                session_id = %session_id,
                "No tracked claude process to interrupt"
            );
        }
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
        let mut map = self.processes.lock().await;
        if let Some(proc) = map.get_mut(session_id) {
            proc.is_running()
        } else {
            false
        }
    }

    async fn cleanup(&self) {
        let mut map = self.processes.lock().await;
        let all_ids: Vec<String> = map.keys().cloned().collect();
        let mut dead = Vec::new();

        for id in &all_ids {
            if let Some(proc) = map.get_mut(id) {
                if !proc.is_running() {
                    dead.push(id.clone());
                }
            }
        }

        for id in &dead {
            map.remove(id);
        }

        if !dead.is_empty() {
            tracing::info!("Cleaned up {} dead claude process(es)", dead.len());
        }
    }

    async fn shutdown(&self) {
        let mut map = self.processes.lock().await;
        let entries: Vec<(String, ClaudeProcess)> = map.drain().collect();

        if entries.is_empty() {
            return;
        }

        tracing::info!("Shutting down {} running claude process(es)", entries.len());
        for (session_id, proc) in entries {
            tracing::info!(session_id = %session_id, "Killing claude process on shutdown");
            process::kill_process(proc).await;
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

