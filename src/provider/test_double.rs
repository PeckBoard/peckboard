//! Registry filler for unit/integration tests. Not a product provider —
//! never registered at boot. Product `mock:*` turns run in the mock WASM
//! plugin. This double exists so tests that only need a catalog + a
//! completing (or `mock:block` parking) turn don't have to load wasm.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::provider::agent::{AgentProvider, ProcessCompletion, SendMessageContext, emit_event};
use crate::provider::registry::{ProviderInfo, ProviderRegistry, standard_effort_levels};
use crate::provider::stream::{CrashKind, ModelInfo, ProviderEvent};

pub struct NoopProvider {
    blocked: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
}

impl NoopProvider {
    pub fn new() -> Self {
        Self {
            blocked: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

fn scenario(model: &str) -> &str {
    let raw = model.strip_prefix("mock:").unwrap_or(model);
    raw.split('@').next().unwrap_or(raw)
}

#[async_trait]
impl AgentProvider for NoopProvider {
    fn id(&self) -> &str {
        "mock"
    }
    fn model_price(&self, model_id: &str) -> Option<(f64, f64)> {
        match model_id {
            "echo" => Some((0.1, 0.5)),
            "happy-path" => Some((1.0, 5.0)),
            _ => None,
        }
    }
    async fn send_message(&self, ctx: SendMessageContext) -> anyhow::Result<()> {
        let conv = format!("mock-{}-1", ctx.session_id);
        emit_event(
            &ctx.db,
            &ctx.broadcaster,
            &ctx.session_id,
            ProviderEvent::Started {
                model: ctx.config.model.clone(),
                conversation_id: Some(conv.clone()),
                metadata: serde_json::Value::Null,
            },
        )
        .await;

        let scenario = scenario(&ctx.config.model);
        if matches!(scenario, "block" | "ask") {
            let stop = Arc::new(AtomicBool::new(false));
            self.blocked
                .lock()
                .await
                .insert(ctx.session_id.clone(), stop.clone());
            let blocked = self.blocked.clone();
            tokio::spawn(async move {
                while !stop.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                blocked.lock().await.remove(&ctx.session_id);
                emit_event(
                    &ctx.db,
                    &ctx.broadcaster,
                    &ctx.session_id,
                    ProviderEvent::Crashed {
                        reason: "interrupted".into(),
                        error_kind: CrashKind::Interrupted,
                        exit_code: None,
                        stderr: None,
                    },
                )
                .await;
                let _ = ctx
                    .completion_tx
                    .send(ProcessCompletion {
                        session_id: ctx.session_id,
                        completed: false,
                        error: Some("interrupted".into()),
                        error_kind: Some(CrashKind::Interrupted),
                        run_id: ctx.run_id,
                        turn_end_only: false,
                    })
                    .await;
            });
            return Ok(());
        }

        emit_event(
            &ctx.db,
            &ctx.broadcaster,
            &ctx.session_id,
            ProviderEvent::Text {
                text: ctx.message.text.clone(),
            },
        )
        .await;
        emit_event(
            &ctx.db,
            &ctx.broadcaster,
            &ctx.session_id,
            ProviderEvent::Completed {
                conversation_id: Some(conv),
                result_meta: serde_json::Value::Null,
            },
        )
        .await;
        let _ = ctx
            .completion_tx
            .send(ProcessCompletion {
                session_id: ctx.session_id,
                completed: true,
                error: None,
                error_kind: None,
                run_id: ctx.run_id,
                turn_end_only: false,
            })
            .await;
        Ok(())
    }
    async fn cancel(&self, session_id: &str) {
        if let Some(stop) = self.blocked.lock().await.get(session_id) {
            stop.store(true, Ordering::SeqCst);
        }
    }
    async fn interrupt(&self, session_id: &str) {
        self.cancel(session_id).await;
    }
    async fn write_stdin(&self, _session_id: &str, _text: &str) -> bool {
        false
    }
    async fn is_running(&self, session_id: &str) -> bool {
        self.blocked.lock().await.contains_key(session_id)
    }
    async fn cleanup(&self) {}
    async fn shutdown(&self) {}
}

pub fn mock_model_infos() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: "echo".into(),
            display_name: "Mock: echo".into(),
            capabilities: vec!["mock".into()],
            tier: 1,
        },
        ModelInfo {
            id: "happy-path".into(),
            display_name: "Mock: happy path".into(),
            capabilities: vec!["mock".into(), "tools".into()],
            tier: 3,
        },
    ]
}

pub async fn register_mock_provider(registry: &ProviderRegistry) {
    registry
        .register(
            Arc::new(NoopProvider::new()),
            ProviderInfo {
                id: "mock".into(),
                display_name: "Mock".into(),
                models: mock_model_infos(),
                effort_levels: standard_effort_levels(),
                capabilities: Default::default(),
            },
        )
        .await;
}
