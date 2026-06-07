use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::sync::mpsc;

use crate::db::Db;
use crate::provider::agent::{ProcessCompletion, SendMessageContext};
use crate::provider::registry::ProviderRegistry;
use crate::provider::stream::SpawnConfig;
use crate::ws::broadcaster::Broadcaster;

/// Default provider id used when a model string has no `provider:` prefix.
const DEFAULT_PROVIDER: &str = "claude";

/// Provider-agnostic dispatcher that owns the registry and routes session
/// operations to the right `AgentProvider` based on the model id.
///
/// Routes/orchestrator code calls this exactly like the previous
/// Claude-specific manager; the only differences are construction
/// (`SessionManager::new(registry)`) and the fact that model strings can
/// now be prefixed (`"claude:opus"`, `"mock:echo"`). Bare strings default
/// to the Claude provider for backward compatibility with stored
/// sessions/cards.
pub struct SessionManager {
    registry: Arc<ProviderRegistry>,
    completion_tx: mpsc::Sender<ProcessCompletion>,
    completion_rx: Arc<Mutex<Option<mpsc::Receiver<ProcessCompletion>>>>,
}

impl SessionManager {
    pub fn new(registry: Arc<ProviderRegistry>) -> Self {
        let (completion_tx, completion_rx) = mpsc::channel(64);
        SessionManager {
            registry,
            completion_tx,
            completion_rx: Arc::new(Mutex::new(Some(completion_rx))),
        }
    }

    /// Take the completion receiver. Called once at startup to set up the
    /// worker-done listener loop.
    pub async fn take_completion_rx(&self) -> Option<mpsc::Receiver<ProcessCompletion>> {
        self.completion_rx.lock().await.take()
    }

    /// Dispatch a new agent run for `session_id`.
    ///
    /// Resolves working dir + resume conversation id from the DB, picks the
    /// provider based on the model prefix, and forwards to its
    /// `AgentProvider::send_message`.
    pub async fn send_message(
        &self,
        session_id: &str,
        message: &str,
        db: &Db,
        broadcaster: &Arc<Broadcaster>,
        config: SpawnConfig,
    ) -> anyhow::Result<()> {
        let session = db
            .get_session(session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("session not found: {}", session_id))?;

        let folder = db
            .get_folder(&session.folder_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("folder not found: {}", session.folder_id))?;

        let working_dir = folder.path.clone();

        let conversation_id = if session.conversation_id.is_some() {
            session.conversation_id.clone()
        } else {
            self.find_conversation_id_from_events(db, session_id).await
        };

        // Session-level model overrides the request, matching the
        // pre-refactor behaviour.
        let final_model = session
            .model
            .clone()
            .unwrap_or_else(|| config.model.clone());

        let (provider_id, _model_id) =
            ProviderRegistry::parse_model_id(&final_model, DEFAULT_PROVIDER);

        let provider = self
            .registry
            .get_provider(&provider_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("unknown agent provider: {}", provider_id))?;

        let final_config = SpawnConfig {
            working_dir,
            model: final_model,
            effort: session.effort.or(config.effort),
            mcp_config_path: config.mcp_config_path,
            env: config.env,
            permission_mode: config.permission_mode,
            timeout_ms: config.timeout_ms,
            metadata: config.metadata,
        };

        let ctx = SendMessageContext {
            session_id: session_id.to_string(),
            message: message.to_string(),
            db: db.clone(),
            broadcaster: broadcaster.clone(),
            config: final_config,
            conversation_id,
            completion_tx: self.completion_tx.clone(),
        };

        provider.send_message(ctx).await
    }

    /// Cancel the run for `session_id` across every registered provider.
    /// Each provider's `cancel` is a no-op if it isn't running that session,
    /// so fan-out is cheap and avoids needing a session→provider map.
    pub async fn cancel(&self, session_id: &str) {
        for info in self.registry.list_providers().await {
            if let Some(p) = self.registry.get_provider(&info.id).await {
                p.cancel(session_id).await;
            }
        }
    }

    pub async fn interrupt(&self, session_id: &str) {
        for info in self.registry.list_providers().await {
            if let Some(p) = self.registry.get_provider(&info.id).await {
                p.interrupt(session_id).await;
            }
        }
    }

    pub async fn write_stdin(&self, session_id: &str, text: &str) -> bool {
        for info in self.registry.list_providers().await {
            if let Some(p) = self.registry.get_provider(&info.id).await {
                if p.write_stdin(session_id, text).await {
                    return true;
                }
            }
        }
        false
    }

    pub async fn is_running(&self, session_id: &str) -> bool {
        for info in self.registry.list_providers().await {
            if let Some(p) = self.registry.get_provider(&info.id).await {
                if p.is_running(session_id).await {
                    return true;
                }
            }
        }
        false
    }

    pub async fn cleanup(&self) {
        for info in self.registry.list_providers().await {
            if let Some(p) = self.registry.get_provider(&info.id).await {
                p.cleanup().await;
            }
        }
    }

    pub async fn shutdown(&self) {
        for info in self.registry.list_providers().await {
            if let Some(p) = self.registry.get_provider(&info.id).await {
                p.shutdown().await;
            }
        }
    }

    /// Scan the event tail for a conversation_id in agent-start or agent-end
    /// events. Used as a fallback when `session.conversation_id` is empty.
    async fn find_conversation_id_from_events(&self, db: &Db, session_id: &str) -> Option<String> {
        let tail = db.events_tail(session_id, 50).await.ok()?;

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
}
