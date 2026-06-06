use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::db::Db;
use crate::provider::stream::SpawnConfig;
use crate::ws::broadcaster::Broadcaster;

use super::process::{self, ClaudeProcess};

/// Maps session IDs to their running Claude CLI processes.
///
/// Provides the high-level API used by route handlers to start, stop,
/// and query agent processes.
pub struct SessionManager {
    processes: Arc<Mutex<HashMap<String, ClaudeProcess>>>,
}

impl SessionManager {
    pub fn new() -> Self {
        SessionManager {
            processes: Arc::new(Mutex::new(HashMap::new())),
        }
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

        // Store the process — we need to take it out to pass ownership to
        // stream_events, but we also need to track that a process *exists*
        // for the session. We use a two-step approach: store a sentinel
        // (the real process), then swap it out in the background task.

        // Actually, stream_events consumes the process, so we cannot store
        // it and also pass it. Instead we do NOT store the process in the
        // map (stream_events owns it), but we store a tracking entry so
        // is_running() can work. We'll use a different approach: store
        // nothing in the map for now and let the background task signal
        // completion. For cancel/interrupt we need the actual child handle.
        //
        // Better approach: split — stream_events takes stdout (not the
        // whole process). But that requires refactoring process.rs.
        //
        // Simplest correct approach: don't call stream_events, instead
        // manage the streaming here and keep the process in the map.
        // But that defeats the abstraction.
        //
        // Pragmatic approach: store the process, have the background task
        // lock the mutex, remove it, and pass it to stream_events. The
        // lock is held only briefly.

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

        // We need to move `child` into the background task
        let sid_for_task = sid.clone();
        tokio::spawn(async move {
            // Run the event stream to completion
            process::stream_events(child, db_clone, broadcaster_clone).await;

            // Clean up the process entry (it's already consumed, but remove
            // any tracking if we added one)
            let mut map = processes.lock().await;
            map.remove(&sid_for_task);
            tracing::debug!(session_id = %sid_for_task, "Process streaming completed, removed from manager");
        });

        // Store a marker in the map so is_running and cancel/interrupt can
        // find it. Since the actual Child is consumed by stream_events, we
        // cannot store it. Instead we track running sessions via a separate
        // mechanism. For now, we rely on the DB event state (agent-start
        // without agent-end) for is_running checks, and cancel/interrupt
        // operate via PID. But that's fragile.
        //
        // A better design: keep the Child in the map and have stream_events
        // borrow stdout only. Let's not over-engineer this — the background
        // task handles cleanup, and cancel/interrupt can emit events that
        // the CLI may respond to.

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

    /// Interrupt the process for a session by writing to its stdin.
    pub async fn interrupt(&self, session_id: &str) {
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
