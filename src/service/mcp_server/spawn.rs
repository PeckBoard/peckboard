//! Production `ExpertDispatcher` backed by the live `AppState`. Lives here
//! (rather than in `context.rs`) so the narrow `ExpertDispatcher` trait the
//! tool layer depends on stays free of any `AppState` coupling — only this
//! impl, constructed in the `mcp` route, pulls the full app in.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::provider::stream::SpawnConfig;
use crate::service::mcp_server::context::ExpertDispatcher;
use crate::state::AppState;

/// Dispatches a capture run on an expert session using the app's
/// `SessionManager`, mirroring the token + MCP-config + locked-dispatch
/// dance the worker orchestrator uses in `spawn_worker_for_card`.
pub struct AppExpertDispatcher {
    state: Arc<AppState>,
}

impl AppExpertDispatcher {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    /// Issue an MCP token + config for `session_id` and build a `SpawnConfig`
    /// for it. Shared by the capture-run and resume paths. `metadata` lets the
    /// caller tag the run (capture vs. ordinary resume). `send_message_locked`
    /// resolves `working_dir` from the session's folder, so an empty string
    /// here is fine.
    async fn spawn_config_for(
        &self,
        session_id: &str,
        metadata: serde_json::Value,
    ) -> anyhow::Result<SpawnConfig> {
        let state = &self.state;
        let session = state
            .db
            .get_session(session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("session not found: {session_id}"))?;

        let mcp_token = state
            .mcp_tokens
            .issue_token(session_id.to_string(), session.project_id.clone())
            .await;

        let mcp_config_path = crate::service::mcp_server::write_mcp_config(
            &state.config.data_dir,
            session_id,
            state.config.port,
            &mcp_token,
        )
        .ok()
        .map(|p| p.to_string_lossy().to_string());

        Ok(SpawnConfig {
            model: session.model.clone().unwrap_or_else(|| "default".into()),
            effort: session.effort.clone(),
            working_dir: String::new(),
            mcp_config_path,
            env: Default::default(),
            permission_mode: Some("bypass".into()),
            timeout_ms: None,
            metadata,
            system_prompt_suffix: None,
        })
    }
}

/// Bridges the plugin layer's [`crate::plugin::host::LiveHost`] to the live
/// app, so a WASM plugin's `peckboard_dispatch_capture` / `_resume_session`
/// host calls schedule real agent runs. Holds a `Weak<AppState>` to avoid an
/// `AppState → PluginManager → LiveHost → AppState` reference cycle, and a
/// runtime `Handle` so its fire-and-forget methods (called from a synchronous
/// WASM host function) can spawn the async dispatch without blocking.
pub struct AppLiveHost {
    state: std::sync::Weak<AppState>,
    rt: tokio::runtime::Handle,
}

impl AppLiveHost {
    pub fn new(state: &Arc<AppState>, rt: tokio::runtime::Handle) -> Self {
        Self {
            state: Arc::downgrade(state),
            rt,
        }
    }
}

impl crate::plugin::host::LiveHost for AppLiveHost {
    fn dispatch_capture(&self, session_id: String, prompt: String) {
        let Some(state) = self.state.upgrade() else {
            return; // app is shutting down — nothing to dispatch to
        };
        self.rt.spawn(async move {
            if let Err(e) = AppExpertDispatcher::new(state)
                .dispatch_capture(&session_id, &prompt)
                .await
            {
                tracing::warn!("plugin dispatch_capture for {session_id} failed: {e}");
            }
        });
    }

    fn resume_session(&self, session_id: String, text: String) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        self.rt.spawn(async move {
            if let Err(e) = AppExpertDispatcher::new(state)
                .resume_session(&session_id, &text)
                .await
            {
                tracing::warn!("plugin resume_session for {session_id} failed: {e}");
            }
        });
    }
}

impl ExpertDispatcher for AppExpertDispatcher {
    fn dispatch_capture<'a>(
        &'a self,
        expert_session_id: &'a str,
        prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let state = &self.state;
            let config = self
                .spawn_config_for(
                    expert_session_id,
                    serde_json::json!({ "expert": true, "capture": true }),
                )
                .await?;

            // Capture is a forced fresh run (re-read the scope), so dispatch
            // through the locked path rather than send_or_queue.
            let lock = state.session_manager.lock_session(expert_session_id).await;
            state
                .session_manager
                .send_message_locked(
                    &lock,
                    crate::provider::message::UserMessage::from_text(prompt),
                    &state.db,
                    &state.broadcaster,
                    config,
                )
                .await?;
            Ok(())
        })
    }

    fn resume_session<'a>(
        &'a self,
        session_id: &'a str,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let state = &self.state;
            let config = self
                .spawn_config_for(session_id, serde_json::Value::Null)
                .await?;

            // Resume exactly like a user message: send_or_queue spawns a fresh
            // run if idle, or queues / injects mid-stream if already running.
            state
                .session_manager
                .send_or_queue(
                    session_id,
                    crate::provider::message::UserMessage::from_text(text),
                    &state.db,
                    &state.broadcaster,
                    config,
                )
                .await?;
            Ok(())
        })
    }
}
