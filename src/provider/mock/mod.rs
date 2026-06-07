use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::provider::agent::{AgentProvider, ProcessCompletion, SendMessageContext, emit_event};
use crate::provider::registry::{ProviderInfo, ProviderRegistry};
use crate::provider::stream::{ModelInfo, ProviderEvent};

/// Mock agent provider. Emits scripted `ProviderEvent` sequences based on
/// the model id, which makes it usable as both a dev-mode stand-in (no
/// `claude` binary required) and as the engine behind e2e tests.
///
/// Recognised model ids (after the `mock:` prefix is stripped):
/// * `echo` — Started → Text(echo of message) → Completed
/// * `happy-path` — Started → Text → ToolStart/ToolEnd → Text → Completed
/// * `tool-use` — Started → ToolStart/ToolEnd (with input/output) → Completed
/// * `crash` — Started → Text → Crashed
/// * `ask` — Started → ControlRequest, waits for stdin → Text(reply) → Completed
pub struct MockProvider {
    runs: Arc<Mutex<HashMap<String, MockRun>>>,
}

struct MockRun {
    handle: JoinHandle<()>,
    stdin_tx: mpsc::Sender<String>,
}

impl MockProvider {
    pub fn new() -> Self {
        MockProvider {
            runs: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentProvider for MockProvider {
    fn id(&self) -> &str {
        "mock"
    }

    async fn send_message(&self, ctx: SendMessageContext) -> anyhow::Result<()> {
        let SendMessageContext {
            session_id,
            message,
            db,
            broadcaster,
            config,
            conversation_id: _,
            completion_tx,
        } = ctx;

        let scenario = config
            .model
            .strip_prefix("mock:")
            .unwrap_or(&config.model)
            .to_string();

        // Kill any prior run for this session.
        {
            let mut runs = self.runs.lock().await;
            if let Some(old) = runs.remove(&session_id) {
                old.handle.abort();
            }
        }

        let (stdin_tx, stdin_rx) = mpsc::channel::<String>(16);
        let runs = self.runs.clone();
        let sid = session_id.clone();
        let model_label = config.model.clone();

        let handle = tokio::spawn(async move {
            let completed = run_scenario(
                &scenario,
                &sid,
                &message,
                &model_label,
                &db,
                &broadcaster,
                stdin_rx,
            )
            .await;

            // Remove the run entry before notifying the orchestrator,
            // so subsequent is_running checks see the right state.
            {
                let mut map = runs.lock().await;
                map.remove(&sid);
            }

            let _ = completion_tx
                .send(ProcessCompletion {
                    session_id: sid.clone(),
                    completed,
                })
                .await;
        });

        let mut runs_map = self.runs.lock().await;
        runs_map.insert(session_id, MockRun { handle, stdin_tx });
        Ok(())
    }

    async fn cancel(&self, session_id: &str) {
        let mut runs = self.runs.lock().await;
        if let Some(run) = runs.remove(session_id) {
            tracing::info!(session_id = %session_id, "Cancelling mock run");
            run.handle.abort();
        }
    }

    async fn interrupt(&self, session_id: &str) {
        // For mocks, an interrupt is just an empty stdin write.
        self.write_stdin(session_id, "").await;
    }

    async fn write_stdin(&self, session_id: &str, text: &str) -> bool {
        let runs = self.runs.lock().await;
        if let Some(run) = runs.get(session_id) {
            run.stdin_tx.try_send(text.to_string()).is_ok()
        } else {
            false
        }
    }

    async fn is_running(&self, session_id: &str) -> bool {
        let runs = self.runs.lock().await;
        runs.get(session_id)
            .map(|r| !r.handle.is_finished())
            .unwrap_or(false)
    }

    async fn cleanup(&self) {
        let mut runs = self.runs.lock().await;
        runs.retain(|_, r| !r.handle.is_finished());
    }

    async fn shutdown(&self) {
        let mut runs = self.runs.lock().await;
        for (_, run) in runs.drain() {
            run.handle.abort();
        }
    }
}

async fn run_scenario(
    scenario: &str,
    session_id: &str,
    message: &str,
    model_label: &str,
    db: &crate::db::Db,
    broadcaster: &crate::ws::broadcaster::Broadcaster,
    mut stdin_rx: mpsc::Receiver<String>,
) -> bool {
    // Tiny pause so consumers can observe events arriving in order.
    let tick = || tokio::time::sleep(Duration::from_millis(5));

    // Every scenario starts with a Started event carrying a synthetic
    // conversation id so resume semantics behave the same as Claude.
    let conv_id = format!("mock-{}", uuid::Uuid::new_v4());
    emit_event(
        db,
        broadcaster,
        session_id,
        ProviderEvent::Started {
            model: model_label.to_string(),
            conversation_id: Some(conv_id.clone()),
            metadata: serde_json::json!({ "scenario": scenario }),
        },
    )
    .await;
    tick().await;

    match scenario {
        "echo" => {
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::Text {
                    text: message.to_string(),
                },
            )
            .await;
        }
        "happy-path" => {
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::Text {
                    text: "Working on it...".into(),
                },
            )
            .await;
            tick().await;
            let tool_id = format!("tool-{}", uuid::Uuid::new_v4());
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::ToolStart {
                    tool_use_id: tool_id.clone(),
                    name: "Bash".into(),
                    input: serde_json::json!({ "command": "echo hello" }),
                },
            )
            .await;
            tick().await;
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::ToolEnd {
                    tool_use_id: tool_id,
                    output: Some("hello".into()),
                    error: None,
                },
            )
            .await;
            tick().await;
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::Text {
                    text: "Done.".into(),
                },
            )
            .await;
        }
        "tool-use" => {
            let tool_id = format!("tool-{}", uuid::Uuid::new_v4());
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::ToolStart {
                    tool_use_id: tool_id.clone(),
                    name: "Read".into(),
                    input: serde_json::json!({ "path": "/tmp/x" }),
                },
            )
            .await;
            tick().await;
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::ToolEnd {
                    tool_use_id: tool_id,
                    output: Some("file contents".into()),
                    error: None,
                },
            )
            .await;
        }
        "crash" => {
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::Text {
                    text: "About to crash".into(),
                },
            )
            .await;
            tick().await;
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::Crashed {
                    reason: "mock scenario crash".into(),
                    exit_code: Some(1),
                    stderr: Some("simulated stderr".into()),
                },
            )
            .await;
            return false;
        }
        "markdown" => {
            // A single assistant text chunk containing markdown features the
            // renderer is expected to handle: heading, bold, list, inline
            // code, and a fenced code block with a language tag (so the
            // syntax highlighter has something to colour).
            let md = "# Hello from mock\n\n\
                      This reply has **bold text**, a list, and a code block.\n\n\
                      - first\n\
                      - second\n\
                      - third\n\n\
                      Inline `mock:markdown` reference.\n\n\
                      ```rust\n\
                      fn main() {\n\
                          println!(\"hi\");\n\
                      }\n\
                      ```\n";
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::Text { text: md.into() },
            )
            .await;
        }
        "ask" => {
            let req_id = uuid::Uuid::new_v4().to_string();
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::ControlRequest {
                    request_id: req_id,
                    request_type: "question".into(),
                    payload: serde_json::json!({ "text": "Continue?" }),
                },
            )
            .await;
            let answer = stdin_rx.recv().await.unwrap_or_default();
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::Text {
                    text: format!("Got reply: {answer}"),
                },
            )
            .await;
        }
        other => {
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::Text {
                    text: format!("unknown mock scenario: {other}"),
                },
            )
            .await;
        }
    }

    emit_event(
        db,
        broadcaster,
        session_id,
        ProviderEvent::Completed {
            conversation_id: Some(conv_id),
        },
    )
    .await;

    true
}

/// Register the mock provider in the registry.
pub async fn register_mock_provider(registry: &ProviderRegistry) {
    let provider = Arc::new(MockProvider::new());
    let models = vec![
        ModelInfo {
            id: "echo".into(),
            display_name: "Mock: echo".into(),
            capabilities: vec!["mock".into()],
        },
        ModelInfo {
            id: "happy-path".into(),
            display_name: "Mock: happy path".into(),
            capabilities: vec!["mock".into(), "tools".into()],
        },
        ModelInfo {
            id: "tool-use".into(),
            display_name: "Mock: tool use".into(),
            capabilities: vec!["mock".into(), "tools".into()],
        },
        ModelInfo {
            id: "crash".into(),
            display_name: "Mock: crash".into(),
            capabilities: vec!["mock".into()],
        },
        ModelInfo {
            id: "ask".into(),
            display_name: "Mock: ask".into(),
            capabilities: vec!["mock".into(), "interactive".into()],
        },
        ModelInfo {
            id: "markdown".into(),
            display_name: "Mock: markdown".into(),
            capabilities: vec!["mock".into(), "markdown".into()],
        },
    ];

    registry
        .register(
            provider,
            ProviderInfo {
                id: "mock".into(),
                display_name: "Mock".into(),
                models,
            },
        )
        .await;
}
