use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::db::Db;
use crate::provider::stream::SpawnConfig;
use crate::ws::broadcaster::Broadcaster;

use super::process::{self, ClaudeProcess};

/// Notification sent when a process finishes streaming.
pub struct ProcessCompletion {
    pub session_id: String,
    pub completed: bool,
}

/// Maps session IDs to their running Claude CLI processes.
///
/// Provides the high-level API used by route handlers to start, stop,
/// and query agent processes.
pub struct SessionManager {
    processes: Arc<Mutex<HashMap<String, ClaudeProcess>>>,
    /// Channels for writing to the stdin of running processes.
    /// When `stream_events` owns the child, callers use this channel
    /// to feed answers (e.g. question-resolved) back into the process.
    stdin_channels: Arc<Mutex<HashMap<String, tokio::sync::mpsc::Sender<String>>>>,
    /// Channel for notifying when a process completes. Used by the
    /// worker orchestrator to trigger handle_worker_done.
    completion_tx: tokio::sync::mpsc::Sender<ProcessCompletion>,
    completion_rx: Arc<Mutex<Option<tokio::sync::mpsc::Receiver<ProcessCompletion>>>>,
}

impl SessionManager {
    pub fn new() -> Self {
        let (completion_tx, completion_rx) = tokio::sync::mpsc::channel(64);
        SessionManager {
            processes: Arc::new(Mutex::new(HashMap::new())),
            stdin_channels: Arc::new(Mutex::new(HashMap::new())),
            completion_tx,
            completion_rx: Arc::new(Mutex::new(Some(completion_rx))),
        }
    }

    /// Take the completion receiver. This should be called once at startup
    /// to set up the worker-done listener loop.
    pub async fn take_completion_rx(&self) -> Option<tokio::sync::mpsc::Receiver<ProcessCompletion>> {
        self.completion_rx.lock().await.take()
    }

    /// Spawn a Claude CLI process for a session and begin streaming its output.
    ///
    /// 1. Looks up the session and its folder to determine the working directory.
    /// 2. Checks the event log for an existing conversation_id (for `--resume`).
    /// 3. Spawns the process and stores it in the map.
    /// 4. Spawns a background task that reads stdout, persists events, and
    ///    broadcasts them. When the task finishes it removes the process from
    ///    the map.
    pub async fn send_message(
        &self,
        session_id: &str,
        message: &str,
        db: &Db,
        broadcaster: &Arc<Broadcaster>,
        config: SpawnConfig,
    ) -> anyhow::Result<()> {
        // Look up the session to get folder_id and any stored conversation_id
        let session = db
            .get_session(session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("session not found: {}", session_id))?;

        // Determine the working directory from the folder path
        let folder = db
            .get_folder(&session.folder_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("folder not found: {}", session.folder_id))?;

        let working_dir = folder.path.clone();

        // Check for an existing conversation_id: prefer the session column,
        // then scan the event tail for an agent-start or agent-end that
        // recorded one.
        let conversation_id = if session.conversation_id.is_some() {
            session.conversation_id.clone()
        } else {
            self.find_conversation_id_from_events(db, session_id).await
        };

        // Build a SpawnConfig with the resolved working directory
        let spawn_config = SpawnConfig {
            working_dir,
            model: if session.model.is_some() {
                session.model.unwrap_or_else(|| config.model.clone())
            } else {
                config.model.clone()
            },
            effort: session.effort.or(config.effort),
            mcp_config_path: config.mcp_config_path,
            env: config.env,
            permission_mode: config.permission_mode,
            timeout_ms: config.timeout_ms,
            metadata: config.metadata,
        };

        // Spawn the child process
        let child = process::spawn_claude(
            session_id,
            message,
            &spawn_config,
            conversation_id.as_deref(),
        )?;

        let sid = session_id.to_string();
        let db_clone = db.clone();
        let broadcaster_clone = broadcaster.clone();
        let processes = self.processes.clone();
        let stdin_channels = self.stdin_channels.clone();

        {
            let mut map = processes.lock().await;
            // If there's already a process for this session, kill it first
            if let Some(old) = map.remove(&sid) {
                tracing::warn!(session_id = %sid, "Killing existing process before spawning new one");
                tokio::spawn(async move {
                    process::kill_process(old).await;
                });
            }
        }

        // Create a channel so callers can write to the process's stdin
        // (e.g. delivering answers to question-resolved events).
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::channel::<String>(32);
        {
            let mut channels = stdin_channels.lock().await;
            channels.insert(sid.clone(), stdin_tx);
        }

        // We need to move `child` into the background task
        let sid_for_task = sid.clone();
        let stdin_channels_for_task = stdin_channels.clone();
        let completion_tx = self.completion_tx.clone();
        tokio::spawn(async move {
            // Run the event stream to completion
            let is_completed = process::stream_events(child, db_clone, broadcaster_clone, stdin_rx).await;

            // Clean up the process entry and stdin channel
            let mut map = processes.lock().await;
            map.remove(&sid_for_task);
            drop(map);

            let mut channels = stdin_channels_for_task.lock().await;
            channels.remove(&sid_for_task);
            drop(channels);

            tracing::debug!(session_id = %sid_for_task, "Process streaming completed, removed from manager");

            // Notify the completion channel so the orchestrator can handle
            // worker-done logic outside this task (avoiding Send issues).
            let _ = completion_tx
                .send(ProcessCompletion {
                    session_id: sid_for_task,
                    completed: is_completed,
                })
                .await;
        });

        Ok(())
    }

    /// Cancel (kill) the process for a session.
    ///
    /// If a process is tracked in the map, kills it. The streaming task will
    /// detect the exit and emit a Crashed event.
    pub async fn cancel(&self, session_id: &str) {
        let mut map = self.processes.lock().await;
        if let Some(proc) = map.remove(session_id) {
            tracing::info!(session_id = %session_id, "Cancelling claude process");
            tokio::spawn(async move {
                process::kill_process(proc).await;
            });
        } else {
            tracing::debug!(session_id = %session_id, "No tracked process to cancel (may have already exited)");
        }
    }

    /// Interrupt the process for a session by writing a newline to its stdin.
    /// Uses the stdin channel if available (preferred), otherwise falls back
    /// to the process handle.
    pub async fn interrupt(&self, session_id: &str) {
        // Try the stdin channel first (covers the common case where
        // stream_events owns the child)
        let sent = self.write_stdin(session_id, "").await;
        if sent {
            tracing::info!(session_id = %session_id, "Sent interrupt via stdin channel");
            return;
        }

        // Fall back to direct process handle
        let mut map = self.processes.lock().await;
        if let Some(proc) = map.get_mut(session_id) {
            process::interrupt_process(proc).await;
        } else {
            tracing::debug!(session_id = %session_id, "No tracked process to interrupt");
        }
    }

    /// Check if there is a live process for a session.
    pub async fn is_running(&self, session_id: &str) -> bool {
        let mut map = self.processes.lock().await;
        if let Some(proc) = map.get_mut(session_id) {
            proc.is_running()
        } else {
            false
        }
    }

    /// Remove any dead processes from the map.
    pub async fn cleanup(&self) {
        let mut map = self.processes.lock().await;

        // Collect all session IDs first, then check each one
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
            tracing::info!("Cleaned up {} dead process(es)", dead.len());
        }
    }

    /// Scan the event tail for a conversation_id in agent-start or agent-end events.
    async fn find_conversation_id_from_events(
        &self,
        db: &Db,
        session_id: &str,
    ) -> Option<String> {
        let tail = db.events_tail(session_id, 50).await.ok()?;

        // Walk backwards to find the most recent conversation_id
        for event in tail.iter().rev() {
            if event.kind == "agent-start" || event.kind == "agent-end" {
                if let Ok(data) = serde_json::from_str::<serde_json::Value>(&event.data) {
                    if let Some(cid) = data.get("conversationId").and_then(|v| v.as_str()) {
                        if !cid.is_empty() {
                            return Some(cid.to_string());
                        }
                    }
                }
            }
        }

        None
    }

    /// Write a message to the stdin of a running process via its channel.
    /// Used to deliver answers (e.g. question-resolved) back to a Claude
    /// process that is waiting for user input.
    pub async fn write_stdin(&self, session_id: &str, text: &str) -> bool {
        let channels = self.stdin_channels.lock().await;
        if let Some(tx) = channels.get(session_id) {
            match tx.try_send(text.to_string()) {
                Ok(()) => {
                    tracing::info!(session_id = %session_id, "Sent stdin message to process");
                    true
                }
                Err(e) => {
                    tracing::warn!(session_id = %session_id, "Failed to send stdin message: {e}");
                    false
                }
            }
        } else {
            tracing::debug!(session_id = %session_id, "No stdin channel for session (process may have exited)");
            false
        }
    }

    /// Kill all tracked processes. Called during graceful shutdown.
    pub async fn shutdown(&self) {
        let mut map = self.processes.lock().await;
        let entries: Vec<(String, ClaudeProcess)> = map.drain().collect();

        if entries.is_empty() {
            return;
        }

        tracing::info!("Shutting down {} running process(es)", entries.len());

        for (session_id, proc) in entries {
            tracing::info!(session_id = %session_id, "Killing process on shutdown");
            process::kill_process(proc).await;
        }
    }
}
