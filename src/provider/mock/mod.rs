use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

use crate::provider::agent::{AgentProvider, ProcessCompletion, SendMessageContext, emit_event};
use crate::provider::registry::{ProviderInfo, ProviderRegistry};
use crate::provider::stream::{ModelInfo, ProviderEvent, ToolImage};

/// Mock agent provider. Emits scripted `ProviderEvent` sequences based on
/// the model id, which makes it usable as both a dev-mode stand-in (no
/// `claude` binary required) and as the engine behind e2e tests.
///
/// Recognised model ids (after the `mock:` prefix is stripped):
/// * `echo` — Started → Text(echo of message) → Completed
/// * `happy-path` — Started → Text → ToolStart/ToolEnd → Text → Completed
/// * `tool-use` — Started → ToolStart/ToolEnd (with input/output) → Completed
/// * `usage` — Started → Text → Edit ToolStart/ToolEnd → ask_expert
///   ToolStart/ToolEnd → Usage(token rollup) → Completed. Drives the usage
///   dashboard (entity rollups, file_update + ask_expert cost breakdowns,
///   token/cost trends) deterministically.
/// * `crash` — Started → Text → Crashed
/// * `ask` — Started → ControlRequest, waits for stdin → Text(reply) → Completed
/// * `todo` — Started → ToolStart/ToolEnd(TodoWrite) → Todo(snapshot) → Completed
/// * `tasks` — Started → scripted TaskCreate/TaskUpdate ToolStart/ToolEnd
///   sequence, each assembled into a Todo snapshot via `TaskTracker` → Completed
pub struct MockProvider {
    runs: Arc<Mutex<HashMap<String, MockRun>>>,
}

struct MockRun {
    handle: JoinHandle<()>,
    stdin_tx: mpsc::Sender<String>,
    cancel: Arc<Notify>,
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
            plugins,
        } = ctx;

        let scenario = config
            .model
            .strip_prefix("mock:")
            .unwrap_or(&config.model)
            .to_string();

        // Notify any prior run for this session to shut down.
        {
            let mut runs = self.runs.lock().await;
            if let Some(old) = runs.remove(&session_id) {
                old.cancel.notify_one();
            }
        }

        let (stdin_tx, stdin_rx) = mpsc::channel::<String>(16);
        let cancel = Arc::new(Notify::new());
        let cancel_for_task = cancel.clone();
        let runs = self.runs.clone();
        let sid = session_id.clone();
        let model_label = config.model.clone();

        // The mock provider scripts text-only scenarios; attachments
        // (if any) ride along in the `UserMessage` but the scripted
        // engine only inspects the text body, matching the
        // pre-multimodal contract for per-turn providers.
        let message_text = message.text;
        let handle = tokio::spawn(async move {
            let completed = run_scenario(
                &scenario,
                &sid,
                &message_text,
                &model_label,
                &db,
                &broadcaster,
                &plugins,
                stdin_rx,
                cancel_for_task,
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
        runs_map.insert(
            session_id,
            MockRun {
                handle,
                stdin_tx,
                cancel,
            },
        );
        Ok(())
    }

    async fn cancel(&self, session_id: &str) {
        // Don't remove the run from `self.runs` here — the scenario task
        // removes itself once `run_scenario` returns. Removing early
        // makes `wait_for_termination` racy: the entry disappears before
        // the synthetic Crashed event is emitted, and any caller that
        // wipes events on the back of the cancel ends up resurrecting
        // a stale "Agent crashed" line.
        let cancel = {
            let runs = self.runs.lock().await;
            runs.get(session_id).map(|r| r.cancel.clone())
        };
        if let Some(c) = cancel {
            tracing::info!(session_id = %session_id, "Cancelling mock run");
            // Notify the run to wind down cleanly so it emits an agent-end
            // (Crashed) event and delivers a ProcessCompletion to the
            // orchestrator. We deliberately do NOT abort the task; the run
            // is short and aborting would skip those signals.
            c.notify_one();
        }
    }

    async fn interrupt(&self, session_id: &str) {
        // Same semantics as `cancel`: actually stop the run. The route
        // handler distinguishes interrupt from cancel by appending a
        // separate event, but at the provider level there is no soft
        // interrupt — the run terminates and the orchestrator gets a
        // completion notification.
        self.cancel(session_id).await;
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

    async fn wait_for_termination(&self, session_id: &str) {
        // Mirrors the Claude provider: the spawned task removes its run
        // from `self.runs` only AFTER `run_scenario` has emitted any
        // synthetic Crashed event. Polling map absence is the signal
        // that the post-cancel events have hit the DB + broadcaster.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if !self.runs.lock().await.contains_key(session_id) {
                return;
            }
            if std::time::Instant::now() >= deadline {
                tracing::warn!(
                    session_id = %session_id,
                    "wait_for_termination timed out; mock run may still be winding down"
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn cleanup(&self) {
        let mut runs = self.runs.lock().await;
        runs.retain(|_, r| !r.handle.is_finished());
    }

    async fn shutdown(&self) {
        let mut runs = self.runs.lock().await;
        for (_, run) in runs.drain() {
            run.cancel.notify_one();
            run.handle.abort();
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_scenario(
    scenario: &str,
    session_id: &str,
    message: &str,
    model_label: &str,
    db: &crate::db::Db,
    broadcaster: &crate::ws::broadcaster::Broadcaster,
    plugins: &crate::plugin::manager::PluginManager,
    mut stdin_rx: mpsc::Receiver<String>,
    cancel: Arc<Notify>,
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

    // Demonstrates the non-Claude provider integration: hand this turn's raw
    // output to any `todo`-hook plugin, which parses it and drives the todo
    // lifecycle. A no-op when no such plugin is installed (the usual case for
    // tests / dev), so it never perturbs the scripted scenarios below.
    crate::plugin::todo_hook::emit_plugin_todos(
        plugins,
        db,
        broadcaster,
        session_id,
        serde_json::json!({ "message": message }),
    )
    .await;

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
                    images: Vec::new(),
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
        "usage" => {
            // A turn that exercises everything the usage dashboard reads:
            // a Read tool call (drives the file_read / cache-read breakdown
            // and the per-turn files_read list), a file-editing tool call
            // (the file_update breakdown), an ask_expert consultation (the
            // ask_expert breakdown), and a per-turn Usage event (the source
            // of every token/cost rollup and trend). Deterministic token
            // counts so e2e assertions are stable.
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::Text {
                    text: "Editing a file and consulting an expert...".into(),
                },
            )
            .await;
            tick().await;
            let read_id = format!("tool-{}", uuid::Uuid::new_v4());
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::ToolStart {
                    tool_use_id: read_id.clone(),
                    name: "Read".into(),
                    input: serde_json::json!({ "file_path": "/workspace/src/lib.rs" }),
                },
            )
            .await;
            tick().await;
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::ToolEnd {
                    tool_use_id: read_id,
                    output: Some("contents".into()),
                    error: None,
                    images: Vec::new(),
                },
            )
            .await;
            tick().await;
            let edit_id = format!("tool-{}", uuid::Uuid::new_v4());
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::ToolStart {
                    tool_use_id: edit_id.clone(),
                    name: "Edit".into(),
                    input: serde_json::json!({ "file_path": "/workspace/src/lib.rs" }),
                },
            )
            .await;
            tick().await;
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::ToolEnd {
                    tool_use_id: edit_id,
                    output: Some("edited".into()),
                    error: None,
                    images: Vec::new(),
                },
            )
            .await;
            tick().await;
            let ask_id = format!("tool-{}", uuid::Uuid::new_v4());
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::ToolStart {
                    tool_use_id: ask_id.clone(),
                    name: "mcp__peckboard__ask_expert".into(),
                    input: serde_json::json!({
                        "area": "src",
                        "question": "How does the usage rollup work?",
                    }),
                },
            )
            .await;
            tick().await;
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::ToolEnd {
                    tool_use_id: ask_id,
                    output: Some("delivered".into()),
                    error: None,
                    images: Vec::new(),
                },
            )
            .await;
            tick().await;
            // End-of-turn token usage. Emitted after the tool calls (usage `ts`
            // is the turn boundary), so the edits/consult above attribute to
            // this turn's cost. Priced at the Opus fallback tier since
            // `mock:usage` isn't a real registry model — so `est_cost` is
            // non-zero, which the dashboard assertions rely on.
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::Usage {
                    input_tokens: 1200,
                    output_tokens: 400,
                    cache_read_tokens: 800,
                    cache_creation_tokens: 200,
                    total_tokens: 2600,
                    context_tokens: 1500,
                    model: Some(model_label.to_string()),
                    turn_seq: None,
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
                    images: Vec::new(),
                },
            )
            .await;
        }
        "screenshot" => {
            // A tool call that returns an image, like the Playwright MCP's
            // `browser_take_screenshot`. Drives the inline-screenshot chat
            // rendering: the tool block should show a thumbnail that opens a
            // full-image lightbox on click. The payload is a 1x1 PNG so the
            // event stays tiny but is a genuinely decodable image.
            const TINY_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::Text {
                    text: "Taking a screenshot...".into(),
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
                    name: "mcp__playwright__browser_take_screenshot".into(),
                    input: serde_json::json!({ "filename": "page.png" }),
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
                    output: Some("Took the screenshot".into()),
                    error: None,
                    images: vec![ToolImage {
                        mime_type: "image/png".into(),
                        data_base64: TINY_PNG.into(),
                    }],
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
        "tool-orphan-crash" => {
            // Emit a ToolStart with no matching ToolEnd, then crash. Used
            // to verify the UI doesn't leave a tool-block spinner running
            // forever when the agent dies before the tool returns.
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::ToolStart {
                    tool_use_id: format!("tool-{}", uuid::Uuid::new_v4()),
                    name: "Bash".into(),
                    input: serde_json::json!({ "command": "sleep forever" }),
                },
            )
            .await;
            tick().await;
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::Crashed {
                    reason: "mock orphan-tool crash".into(),
                    exit_code: Some(1),
                    stderr: None,
                },
            )
            .await;
            return false;
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
            let answer = tokio::select! {
                _ = cancel.notified() => None,
                ans = stdin_rx.recv() => ans,
            };
            // `None` from the cancel branch (or, defensively, from a
            // closed stdin channel) means the run was cancelled before
            // an answer arrived — emit Crashed and report not-completed.
            let Some(text) = answer else {
                emit_event(
                    db,
                    broadcaster,
                    session_id,
                    ProviderEvent::Crashed {
                        reason: "interrupted".into(),
                        exit_code: None,
                        stderr: None,
                    },
                )
                .await;
                return false;
            };
            let answer = text;
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
        "todo" => {
            // Emit a TodoWrite tool call exactly as Claude would, then run it
            // through the same `snapshot_from_tool_call` seam the real provider
            // uses so the normalized `todo` event is byte-for-byte consistent.
            let raw_input = serde_json::json!({
                "todos": [
                    { "content": "Write the parser", "status": "completed", "activeForm": "Writing the parser" },
                    { "content": "Wire up the route", "status": "in_progress", "activeForm": "Wiring up the route" },
                    { "content": "Add tests", "status": "pending", "activeForm": "Adding tests" },
                ]
            });
            let tool_id = format!("tool-{}", uuid::Uuid::new_v4());
            emit_event(
                db,
                broadcaster,
                session_id,
                ProviderEvent::ToolStart {
                    tool_use_id: tool_id.clone(),
                    name: "TodoWrite".into(),
                    input: raw_input.clone(),
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
                    output: Some("Todos updated".into()),
                    error: None,
                    images: Vec::new(),
                },
            )
            .await;
            tick().await;
            if let Some(snapshot) = crate::todo::snapshot_from_tool_call("TodoWrite", &raw_input) {
                emit_event(
                    db,
                    broadcaster,
                    session_id,
                    ProviderEvent::Todo {
                        todos: snapshot.todos,
                    },
                )
                .await;
            }
        }
        "tasks" => {
            // Claude Code ≥ 2.1 reports work items via incremental
            // TaskCreate/TaskUpdate calls instead of TodoWrite. Script the
            // same tool sequence the real CLI emits and drive it through the
            // same `TaskTracker` seam the process loop uses, so the
            // normalized `todo` events stay byte-for-byte consistent. Final
            // state matches `mock:todo`: parser done, route in progress,
            // tests pending.
            let script: Vec<(&str, serde_json::Value, &str, serde_json::Value)> = vec![
                (
                    "TaskCreate",
                    serde_json::json!({
                        "subject": "Write the parser",
                        "description": "Parse the stream",
                        "activeForm": "Writing the parser",
                    }),
                    "Task #1 created successfully: Write the parser",
                    serde_json::json!({ "task": { "id": "1", "subject": "Write the parser" } }),
                ),
                (
                    "TaskCreate",
                    serde_json::json!({
                        "subject": "Wire up the route",
                        "description": "Expose it over HTTP",
                        "activeForm": "Wiring up the route",
                    }),
                    "Task #2 created successfully: Wire up the route",
                    serde_json::json!({ "task": { "id": "2", "subject": "Wire up the route" } }),
                ),
                (
                    "TaskCreate",
                    serde_json::json!({
                        "subject": "Add tests",
                        "description": "Lock in behaviour",
                        "activeForm": "Adding tests",
                    }),
                    "Task #3 created successfully: Add tests",
                    serde_json::json!({ "task": { "id": "3", "subject": "Add tests" } }),
                ),
                (
                    "TaskUpdate",
                    serde_json::json!({ "taskId": "1", "status": "in_progress" }),
                    "Updated task #1 status",
                    serde_json::json!({
                        "success": true,
                        "taskId": "1",
                        "statusChange": { "from": "pending", "to": "in_progress" },
                    }),
                ),
                (
                    "TaskUpdate",
                    serde_json::json!({ "taskId": "1", "status": "completed" }),
                    "Updated task #1 status",
                    serde_json::json!({
                        "success": true,
                        "taskId": "1",
                        "statusChange": { "from": "in_progress", "to": "completed" },
                    }),
                ),
                (
                    "TaskUpdate",
                    serde_json::json!({ "taskId": "2", "status": "in_progress" }),
                    "Updated task #2 status",
                    serde_json::json!({
                        "success": true,
                        "taskId": "2",
                        "statusChange": { "from": "pending", "to": "in_progress" },
                    }),
                ),
            ];
            let mut tracker = crate::todo::TaskTracker::new();
            for (name, input, output, result) in script {
                let tool_id = format!("tool-{}", uuid::Uuid::new_v4());
                emit_event(
                    db,
                    broadcaster,
                    session_id,
                    ProviderEvent::ToolStart {
                        tool_use_id: tool_id.clone(),
                        name: name.into(),
                        input: input.clone(),
                    },
                )
                .await;
                tracker.on_tool_start(&tool_id, name, &input);
                tick().await;
                emit_event(
                    db,
                    broadcaster,
                    session_id,
                    ProviderEvent::ToolEnd {
                        tool_use_id: tool_id.clone(),
                        output: Some(output.into()),
                        error: None,
                        images: Vec::new(),
                    },
                )
                .await;
                if let Some(snapshot) = tracker.on_tool_end(&tool_id, false, Some(&result)) {
                    emit_event(
                        db,
                        broadcaster,
                        session_id,
                        ProviderEvent::Todo {
                            todos: snapshot.todos,
                        },
                    )
                    .await;
                }
                tick().await;
            }
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

/// Scripted models the mock provider exposes. Pulled out of
/// `register_mock_provider` so the built-in `MockPlugin`
/// (`src/plugin/builtins/mock.rs`) can call it without duplicating the
/// list.
pub fn mock_model_infos() -> Vec<ModelInfo> {
    vec![
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
            id: "usage".into(),
            display_name: "Mock: usage".into(),
            capabilities: vec!["mock".into(), "tools".into()],
        },
        ModelInfo {
            id: "crash".into(),
            display_name: "Mock: crash".into(),
            capabilities: vec!["mock".into()],
        },
        ModelInfo {
            id: "tool-orphan-crash".into(),
            display_name: "Mock: tool start without end then crash".into(),
            capabilities: vec!["mock".into(), "tools".into()],
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
    ]
}

/// Register the mock provider in the registry directly. Kept for tests
/// that build a bare `ProviderRegistry` without going through the plugin
/// catalog; production code paths flow through `MockPlugin`.
pub async fn register_mock_provider(registry: &ProviderRegistry) {
    registry
        .register(
            Arc::new(MockProvider::new()),
            ProviderInfo {
                id: "mock".into(),
                display_name: "Mock".into(),
                models: mock_model_infos(),
                // Mirrors the plugin-registered mock provider: the standard
                // ladder so effort-picker e2e/tests have a deterministic
                // provider that offers effort levels.
                effort_levels: crate::provider::registry::standard_effort_levels(),
            },
        )
        .await;
}
