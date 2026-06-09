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
}

impl ExpertDispatcher for AppExpertDispatcher {
    fn dispatch_capture<'a>(
        &'a self,
        expert_session_id: &'a str,
        prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let state = &self.state;
            let session = state
                .db
                .get_session(expert_session_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("expert session not found: {expert_session_id}"))?;

            let mcp_token = state
                .mcp_tokens
                .issue_token(expert_session_id.to_string(), session.project_id.clone())
                .await;

            let mcp_config_path = crate::service::mcp_server::write_mcp_config(
                &state.config.data_dir,
                expert_session_id,
                state.config.port,
                &mcp_token,
            )
            .ok()
            .map(|p| p.to_string_lossy().to_string());

            // `send_message_locked` resolves working_dir from the session's
            // folder, so an empty string here is fine.
            let config = SpawnConfig {
                model: session.model.clone().unwrap_or_else(|| "default".into()),
                effort: session.effort.clone(),
                working_dir: String::new(),
                mcp_config_path,
                env: Default::default(),
                permission_mode: Some("bypass".into()),
                timeout_ms: None,
                metadata: serde_json::json!({ "expert": true, "capture": true }),
                system_prompt_suffix: None,
            };

            let lock = state.session_manager.lock_session(expert_session_id).await;
            state
                .session_manager
                .send_message_locked(&lock, prompt, &state.db, &state.broadcaster, config)
                .await?;
            Ok(())
        })
    }
}
