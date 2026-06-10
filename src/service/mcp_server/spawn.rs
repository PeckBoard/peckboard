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
            // `send_message_locked` re-derives this from the session
            // (question-experts run answer-only), so the value here is moot.
            restrict_to_qa: false,
        })
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
