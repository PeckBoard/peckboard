//! Sync port of `src/provider/mock/mod.rs::run_scenario`.
//!
//! Emits the same `ProviderEvent` JSON the host deserializes
//! (`#[serde(tag = "kind", rename_all = "snake_case")]`).

use std::cell::Cell;

use serde_json::{Value, json};

use crate::host::{self, HostFn};

pub fn run_scenario(payload: &Value) -> Result<(), String> {
    let session_id = payload
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if session_id.is_empty() {
        return Err("provider.send payload missing session_id".into());
    }
    let model = payload
        .get("spawn_config")
        .and_then(|c| c.get("model"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let working_dir = payload
        .get("spawn_config")
        .and_then(|c| c.get("working_dir"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let message = payload
        .get("message")
        .and_then(|m| m.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let resume = payload.get("conversation_id").and_then(|v| v.as_str());

    let raw = model.strip_prefix("mock:").unwrap_or(model);
    let scenario = raw.split('@').next().unwrap_or(raw);

    // A fresh conversation id per turn (persisted counter), so resume
    // semantics behave the same as Claude: a cold replay after a rejected
    // resume lands on a NEW id, never the dead one.
    let turn = {
        let prev = host::call_host(
            HostFn::StoreGet,
            &json!({ "collection": "mock-turns", "key": session_id }),
        )
        .ok()
        .and_then(|v| {
            v.get("value")
                .and_then(|d| d.get("n"))
                .and_then(|n| n.as_i64())
        })
        .unwrap_or(0);
        let n = prev + 1;
        let _ = host::call_host(
            HostFn::StorePut,
            &json!({ "collection": "mock-turns", "key": session_id, "data": { "n": n } }),
        );
        n
    };

    let mut ctx = Ctx {
        session_id,
        model,
        working_dir,
        message,
        resume,
        scenario,
        conv_id: format!("mock-{session_id}-{turn}"),
        turn,
        n: 0,
        aborted: Cell::new(false),
    };
    ctx.run()
}

struct Ctx<'a> {
    session_id: &'a str,
    model: &'a str,
    working_dir: &'a str,
    message: &'a str,
    resume: Option<&'a str>,
    scenario: &'a str,
    conv_id: String,
    turn: i64,
    n: u32,
    aborted: Cell<bool>,
}

impl Ctx<'_> {
    fn emit(&self, event: Value) -> Result<(), String> {
        host::call_host(
            HostFn::EmitProviderEvent,
            &json!({
                "session_id": self.session_id,
                "event": event,
            }),
        )
        .map(|_| ())
    }

    fn tool_id(&mut self) -> String {
        self.n += 1;
        format!("tool-{}-{}-{}", self.session_id, self.turn, self.n)
    }

    fn should_stop(&self) -> bool {
        host::call_host(
            HostFn::ProviderShouldStop,
            &json!({ "session_id": self.session_id }),
        )
        .ok()
        .and_then(|v| v.get("stop").and_then(|s| s.as_bool()))
        .unwrap_or(true)
    }

    fn interrupted(&self) -> Result<(), String> {
        self.aborted.set(true);
        self.emit(json!({
            "kind": "crashed",
            "reason": "interrupted",
            "error_kind": "interrupted",
            "exit_code": null,
            "stderr": null,
        }))
    }

    /// Returns `false` when the turn was aborted (Crashed already emitted).
    fn tick(&self) -> Result<bool, String> {
        if self.aborted.get() {
            return Ok(false);
        }
        if self.should_stop() {
            self.interrupted()?;
            return Ok(false);
        }
        Ok(true)
    }

    fn emit_text(&self, text: &str) -> Result<(), String> {
        self.emit(json!({ "kind": "text", "text": text }))
    }

    fn emit_thinking(&self, text: &str) -> Result<(), String> {
        self.emit(json!({ "kind": "thinking", "text": text }))
    }

    fn tool_start(&self, id: &str, name: &str, input: Value) -> Result<(), String> {
        self.emit(json!({
            "kind": "tool_start",
            "tool_use_id": id,
            "name": name,
            "input": input,
        }))
    }

    fn tool_end(
        &self,
        id: &str,
        output: Option<String>,
        error: Option<String>,
        images: Value,
    ) -> Result<(), String> {
        self.emit(json!({
            "kind": "tool_end",
            "tool_use_id": id,
            "output": output,
            "error": error,
            "images": images,
        }))
    }

    fn tool_end_ok(&self, id: &str, output: &str) -> Result<(), String> {
        self.tool_end(id, Some(output.to_string()), None, json!([]))
    }

    fn completed(&self, result_meta: Value) -> Result<(), String> {
        self.emit(json!({
            "kind": "completed",
            "conversation_id": self.conv_id,
            "result_meta": result_meta,
        }))
    }

    fn crashed(
        &self,
        reason: &str,
        error_kind: &str,
        exit_code: Option<i32>,
        stderr: Option<&str>,
    ) -> Result<(), String> {
        self.emit(json!({
            "kind": "crashed",
            "reason": reason,
            "error_kind": error_kind,
            "exit_code": exit_code,
            "stderr": stderr,
        }))
    }

    /// Host blocks until stdin text or cooperative stop.
    fn read_stdin(&self) -> Result<Option<String>, String> {
        let v = host::call_host(
            HostFn::ProviderReadStdin,
            &json!({ "session_id": self.session_id }),
        )?;
        if v.get("stopped").and_then(|s| s.as_bool()) == Some(true) {
            return Ok(None);
        }
        if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
            return Ok(Some(text.to_string()));
        }
        Ok(None)
    }

    fn wait_until_stop(&self) -> Result<(), String> {
        loop {
            match self.read_stdin()? {
                None => {
                    self.interrupted()?;
                    return Ok(());
                }
                Some(_) => continue,
            }
        }
    }

    /// Run a real MCP tool with this turn's context, with no chat tool
    /// events — the shape `plan-review` needs (native wrote the plan rows
    /// directly, with nothing in the tool lane).
    fn invoke_mcp(&self, name: &str, args: Value) -> Result<Value, String> {
        host::call_host(
            HostFn::ProviderInvokeMcp,
            &json!({
                "session_id": self.session_id,
                "name": name,
                "arguments": args,
            }),
        )
    }

    /// Port of native `call_mcp_tool`: ToolStart → REAL MCP handler →
    /// ToolEnd, returning the handler's value on success. A gate refusal or
    /// handler error lands in the ToolEnd's `error`, not an abort.
    fn call_mcp_tool(&mut self, name: &str, args: Value) -> Result<Option<Value>, String> {
        let id = self.tool_id();
        self.tool_start(&id, &format!("mcp__peckboard__{name}"), args.clone())?;
        let (result, output, error) = match self.invoke_mcp(name, args) {
            Ok(v) if v.get("ok").and_then(|o| o.as_bool()) == Some(true) => {
                let r = v.get("result").cloned().unwrap_or(Value::Null);
                (Some(r.clone()), Some(r.to_string()), None)
            }
            Ok(v) => (
                None,
                None,
                Some(
                    v.get("error")
                        .and_then(|e| e.as_str())
                        .unwrap_or("tool failed")
                        .to_string(),
                ),
            ),
            Err(e) => (None, None, Some(e)),
        };
        self.tool_end(&id, output, error, json!([]))?;
        Ok(result)
    }

    fn emit_todo(&self, todos: Value) -> Result<(), String> {
        self.emit(json!({ "kind": "todo", "todos": todos }))
    }

    fn run(&mut self) -> Result<(), String> {
        self.emit(json!({
            "kind": "started",
            "model": self.model,
            "conversation_id": self.conv_id,
            "metadata": {
                "scenario": self.scenario,
                "working_dir": self.working_dir,
            },
        }))?;
        if !self.tick()? {
            return Ok(());
        }

        match self.scenario {
            "echo" => self.echo()?,
            "happy-path" => self.happy_path()?,
            "run-command" => self.run_command()?,
            "subagent" => self.subagent()?,
            "usage" => self.usage()?,
            "tool-use" => self.tool_use()?,
            "cli-tools" => self.cli_tools()?,
            "mcp" => self.mcp_blocks()?,
            "tool-error" => self.tool_error()?,
            "system-blob" => self.system_blob()?,
            "screenshot" => self.screenshot()?,
            "diff" => self.diff()?,
            "subagent-native" => self.subagent_native()?,
            "thinking" => self.thinking()?,
            "tool-orphan-crash" => return self.tool_orphan_crash(),
            "crash" => return self.crash(),
            "auth-error" => return self.auth_error(),
            "auth-error-once" => {
                if self.auth_error_once()? {
                    return Ok(());
                }
            }
            "resume-error" => {
                if self.resume_error()? {
                    return Ok(());
                }
            }
            "markdown" => self.markdown()?,
            "ask" => return self.ask(),
            "block" => return self.block(),
            "plan-review" => self.plan_review()?,
            "doc-review" => return self.doc_review(),
            "todo" => self.todo()?,
            "tasks" => self.tasks()?,
            "ctx" => self.ctx()?,
            other => {
                self.emit_text(&format!("unknown mock scenario: {other}"))?;
            }
        }

        if self.aborted.get() {
            return Ok(());
        }
        if self.should_stop() {
            return self.interrupted();
        }
        self.completed(Value::Null)
    }
    /// Generic MCP driver: every ```mcp fenced block in the message is a
    /// `{"tool": name, "args": {...}}` request run against the REAL MCP
    /// handler with this session's scope. Lets e2e specs exercise any
    /// plugin tool end to end (e.g. ui-gauge page generation) by embedding
    /// the call in the dispatched prompt.
    fn mcp_blocks(&mut self) -> Result<(), String> {
        let blocks = extract_mcp_blocks(self.message);
        if blocks.is_empty() {
            return self.emit_text("no ```mcp blocks in message");
        }
        let total = blocks.len();
        let mut ran = 0usize;
        for b in blocks {
            let Some(tool) = b.get("tool").and_then(|t| t.as_str()) else {
                self.emit_text("mcp block missing 'tool'")?;
                continue;
            };
            let args = b.get("args").cloned().unwrap_or(json!({}));
            if self.call_mcp_tool(tool, args)?.is_some() {
                ran += 1;
            }
            if !self.tick()? {
                return Ok(());
            }
        }
        self.emit_text(&format!("ran {ran}/{total} mcp block(s)"))
    }

    fn echo(&self) -> Result<(), String> {
        self.emit_text(self.message)
    }

    fn happy_path(&mut self) -> Result<(), String> {
        self.emit_text("Working on it...")?;
        if !self.tick()? {
            return Ok(());
        }
        let tool_id = self.tool_id();
        self.tool_start(
            &tool_id,
            "Bash",
            json!({ "command": "echo hello", "description": "Say hello to prove the shell works." }),
        )?;
        if !self.tick()? {
            return Ok(());
        }
        self.tool_end_ok(&tool_id, "hello")?;
        if !self.tick()? {
            return Ok(());
        }
        self.emit_text("Done.")
    }

    fn run_command(&mut self) -> Result<(), String> {
        self.emit_text("Building the release binary...")?;
        if !self.tick()? {
            return Ok(());
        }
        let tool_id = self.tool_id();
        self.tool_start(
            &tool_id,
            "mcp__peckboard__run_command",
            json!({
                "command": "cargo",
                "args": ["build", "--release"],
                "reason": "Build the release binary to verify the change compiles.",
            }),
        )?;
        if !self.tick()? {
            return Ok(());
        }
        self.tool_end_ok(&tool_id, "Finished `release` profile")?;
        if !self.tick()? {
            return Ok(());
        }
        self.emit_text("Build complete.")
    }

    fn subagent(&mut self) -> Result<(), String> {
        let child_id = self.message.trim();
        let tool_id = self.tool_id();
        self.tool_start(
            &tool_id,
            "mcp__peckboard__spawn_subagent",
            json!({ "name": "child", "prompt": "Do the thing." }),
        )?;
        if !self.tick()? {
            return Ok(());
        }
        self.tool_end_ok(
            &tool_id,
            &json!({ "subagent_session_id": child_id }).to_string(),
        )?;
        if !self.tick()? {
            return Ok(());
        }
        self.emit_text("Subagent spawned.")
    }

    /// A Claude-style built-in Task/Agent subagent: the child's events are
    /// stamped with the Agent call's `parent_tool_use_id`, exactly as the
    /// Claude parser does for sidechain frames.
    fn subagent_native(&mut self) -> Result<(), String> {
        const PARENT: &str = "toolu_native_1";
        let child = |mut event: Value| {
            event["parent_tool_use_id"] = json!(PARENT);
            event
        };
        self.tool_start(
            PARENT,
            "Agent",
            json!({
                "description": "Explore repo",
                "prompt": "Look around",
                "subagent_type": "Explore",
            }),
        )?;
        if !self.tick()? {
            return Ok(());
        }
        self.emit(child(
            json!({ "kind": "text", "text": "Child is looking around" }),
        ))?;
        if !self.tick()? {
            return Ok(());
        }
        let read_id = self.tool_id();
        self.emit(child(json!({
            "kind": "tool_start",
            "tool_use_id": read_id,
            "name": "Read",
            "input": { "file_path": "README.md" },
        })))?;
        if !self.tick()? {
            return Ok(());
        }
        self.emit(child(json!({
            "kind": "tool_end",
            "tool_use_id": read_id,
            "output": "# README",
            "error": null,
            "images": [],
        })))?;
        if !self.tick()? {
            return Ok(());
        }
        self.tool_end_ok(PARENT, "Child finished")?;
        if !self.tick()? {
            return Ok(());
        }
        self.emit_text("Parent done")
    }

    fn usage(&mut self) -> Result<(), String> {
        self.emit_text("Editing a file and consulting an expert...")?;
        if !self.tick()? {
            return Ok(());
        }
        let read_id = self.tool_id();
        self.tool_start(
            &read_id,
            "Read",
            json!({ "file_path": "/workspace/src/lib.rs" }),
        )?;
        if !self.tick()? {
            return Ok(());
        }
        self.tool_end_ok(&read_id, "contents")?;
        if !self.tick()? {
            return Ok(());
        }
        let edit_id = self.tool_id();
        self.tool_start(
            &edit_id,
            "Edit",
            json!({ "file_path": "/workspace/src/lib.rs" }),
        )?;
        if !self.tick()? {
            return Ok(());
        }
        self.tool_end_ok(&edit_id, "edited")?;
        if !self.tick()? {
            return Ok(());
        }
        let ask_id = self.tool_id();
        self.tool_start(
            &ask_id,
            "mcp__peckboard__ask_expert",
            json!({
                "area": "src",
                "question": "How does the usage rollup work?",
            }),
        )?;
        if !self.tick()? {
            return Ok(());
        }
        self.tool_end_ok(&ask_id, "delivered")?;
        if !self.tick()? {
            return Ok(());
        }
        self.emit(json!({
            "kind": "usage",
            "input_tokens": 1200,
            "output_tokens": 400,
            "cache_read_tokens": 800,
            "cache_creation_tokens": 200,
            "total_tokens": 2600,
            "context_tokens": 1500,
            "model": self.model,
            "turn_seq": null,
        }))
    }

    fn tool_use(&mut self) -> Result<(), String> {
        let tool_id = self.tool_id();
        self.tool_start(&tool_id, "Read", json!({ "path": "/tmp/x" }))?;
        if !self.tick()? {
            return Ok(());
        }
        self.tool_end_ok(&tool_id, "file contents")
    }

    fn cli_tools(&mut self) -> Result<(), String> {
        let shell_id = self.tool_id();
        let mcp_id = self.tool_id();
        let list_id = self.tool_id();
        let read_id = self.tool_id();
        let edit_id = self.tool_id();
        let path = "/workspace/src/lib.rs";
        let events = vec![
            json!({
                "kind": "tool_start",
                "tool_use_id": shell_id,
                "name": "shell",
                "input": {
                    "command": "cargo build --release",
                    "reason": "Build the release binary",
                },
            }),
            json!({
                "kind": "tool_end",
                "tool_use_id": shell_id,
                "output": json!({ "stdout": "Finished release\n", "exitCode": 0 }).to_string(),
                "error": null,
                "images": [],
            }),
            json!({
                "kind": "tool_start",
                "tool_use_id": mcp_id,
                "name": "mcp__peckboard__search_files",
                "input": { "query": "needle", "path_contains": "src" },
            }),
            json!({
                "kind": "tool_end",
                "tool_use_id": mcp_id,
                "output": "one match",
                "error": null,
                "images": [],
            }),
            json!({
                "kind": "tool_start",
                "tool_use_id": list_id,
                "name": "getMcpTools",
                "input": { "server": "peckboard" },
            }),
            json!({
                "kind": "tool_end",
                "tool_use_id": list_id,
                "output": "2 tools",
                "error": null,
                "images": [],
            }),
            json!({
                "kind": "tool_start",
                "tool_use_id": read_id,
                "name": "read",
                "input": { "path": path },
            }),
            json!({
                "kind": "tool_end",
                "tool_use_id": read_id,
                "output": "fn main() {}",
                "error": null,
                "images": [],
            }),
            json!({
                "kind": "tool_start",
                "tool_use_id": edit_id,
                "name": "edit",
                "input": { "path": path, "streamContent": "fn main() {}\n" },
            }),
            json!({
                "kind": "file_diff",
                "path": path,
                "diff": format!("--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-old\n+new"),
                "added": 1,
                "removed": 1,
                "created": false,
            }),
            json!({
                "kind": "tool_end",
                "tool_use_id": edit_id,
                "output": "updated",
                "error": null,
                "images": [],
            }),
        ];
        for event in events {
            self.emit(event)?;
            if !self.tick()? {
                return Ok(());
            }
        }
        Ok(())
    }

    fn tool_error(&mut self) -> Result<(), String> {
        let tool_id = self.tool_id();
        self.tool_start(
            &tool_id,
            "run_command",
            json!({
                "command": "nope",
                "reason": "check the thing",
            }),
        )?;
        if !self.tick()? {
            return Ok(());
        }
        self.tool_end(
            &tool_id,
            None,
            Some("command not found: nope".into()),
            json!([]),
        )
    }

    fn system_blob(&self) -> Result<(), String> {
        // No `text` key at all: the chat must show a label with the payload
        // behind a <details>, not a raw object blob in the feed — which only
        // happens when the provider supplied no text.
        self.emit(json!({
            "kind": "system",
            "subtype": "mock_blob",
            "detail": {
                "code": 42,
                "payload": { "reason": "mock system blob" },
            },
        }))
    }

    fn screenshot(&mut self) -> Result<(), String> {
        const TINY_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        self.emit_text("Taking a screenshot...")?;
        if !self.tick()? {
            return Ok(());
        }
        let tool_id = self.tool_id();
        self.tool_start(
            &tool_id,
            "mcp__playwright__browser_take_screenshot",
            json!({ "filename": "page.png" }),
        )?;
        if !self.tick()? {
            return Ok(());
        }
        self.tool_end(
            &tool_id,
            Some("Took the screenshot".into()),
            None,
            json!([{ "mime_type": "image/png", "data_base64": TINY_PNG }]),
        )?;
        if !self.tick()? {
            return Ok(());
        }
        self.emit_text("Done.")
    }

    fn diff(&mut self) -> Result<(), String> {
        let tool_id = self.tool_id();
        self.tool_start(
            &tool_id,
            "mcp__peckboard__edit_file",
            json!({ "path": "src/demo.ts", "original_hash": "abc" }),
        )?;
        if !self.tick()? {
            return Ok(());
        }
        self.emit(json!({
            "kind": "file_diff",
            "path": "src/demo.ts",
            "diff": "@@ -1,3 +1,3 @@\n context\n-old line\n+new line\n context",
            "added": 1,
            "removed": 1,
            "created": false,
        }))?;
        if !self.tick()? {
            return Ok(());
        }
        self.tool_end_ok(
            &tool_id,
            "{\"ok\":true,\"path\":\"src/demo.ts\",\"edits_applied\":1}",
        )?;
        if !self.tick()? {
            return Ok(());
        }
        self.emit_text("Edited src/demo.ts.")
    }

    fn thinking(&self) -> Result<(), String> {
        self.emit_thinking("Let me reason about this. ")?;
        if !self.tick()? {
            return Ok(());
        }
        self.emit_thinking("The answer is clearly 42.")?;
        if !self.tick()? {
            return Ok(());
        }
        self.emit_text("The answer is 42.")
    }

    fn tool_orphan_crash(&mut self) -> Result<(), String> {
        let tool_id = self.tool_id();
        self.tool_start(&tool_id, "Bash", json!({ "command": "sleep forever" }))?;
        if !self.tick()? {
            return Ok(());
        }
        self.crashed("mock orphan-tool crash", "unknown", Some(1), None)
    }

    fn crash(&self) -> Result<(), String> {
        self.emit_text("About to crash")?;
        if !self.tick()? {
            return Ok(());
        }
        self.crashed(
            "mock scenario crash",
            "unknown",
            Some(1),
            Some("simulated stderr"),
        )
    }

    fn auth_error(&self) -> Result<(), String> {
        self.completed(json!({
            "error": "Failed to authenticate: OAuth session expired and could not be refreshed",
            "errorKind": "auth_expired",
        }))
    }

    /// `true` if this call already emitted a terminal (first-turn fail).
    fn auth_error_once(&self) -> Result<bool, String> {
        let already_failed = host::call_host(
            HostFn::StoreGet,
            &json!({
                "collection": "auth-error-once",
                "key": self.session_id,
            }),
        )
        .ok()
        .and_then(|v| v.get("value").cloned())
        .and_then(|v| v.get("failed").and_then(|f| f.as_bool()))
        .unwrap_or(false);
        if !already_failed {
            let _ = host::call_host(
                HostFn::StorePut,
                &json!({
                    "collection": "auth-error-once",
                    "key": self.session_id,
                    "data": { "failed": true },
                }),
            );
            self.completed(json!({
                "error": "Failed to authenticate. API Error: 401 OAuth access token has been revoked.",
                "errorKind": "auth_expired",
            }))?;
            return Ok(true);
        }
        self.emit_text("Authenticated on the retry.")?;
        Ok(false)
    }

    /// `true` if this call already emitted a terminal (resume rejected).
    fn resume_error(&self) -> Result<bool, String> {
        if let Some(dead) = self.resume {
            self.emit(json!({
                "kind": "completed",
                "conversation_id": null,
                "result_meta": {
                    "error": format!("no rollout found for thread id {dead}"),
                    "errorKind": "resume_failed",
                },
            }))?;
            return Ok(true);
        }
        self.emit_text("Started a fresh conversation.")?;
        Ok(false)
    }

    fn markdown(&self) -> Result<(), String> {
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
        self.emit_text(md)
    }

    fn ask(&mut self) -> Result<(), String> {
        let req_id = self.tool_id();
        self.emit(json!({
            "kind": "control_request",
            "request_id": req_id,
            "request_type": "question",
            "payload": { "text": "Continue?" },
        }))?;
        let Some(answer) = self.read_stdin()? else {
            return self.interrupted();
        };
        self.emit_text(&format!("Got reply: {answer}"))?;
        self.completed(Value::Null)
    }

    fn block(&self) -> Result<(), String> {
        self.emit_text("working…")?;
        if !self.tick()? {
            return Ok(());
        }
        self.wait_until_stop()
    }

    fn plan_review(&self) -> Result<(), String> {
        // Drives the real plan-persistence path (the `propose_plan` MCP
        // handler) so e2e can verify a saved plan survives clears/switches
        // without a real model. The handler links the plan to the session's
        // card/project and broadcasts `plan-proposed`, like native did.
        self.emit_text("Writing the plan…")?;
        if !self.tick()? {
            return Ok(());
        }
        let markdown = "# Widget plan\n\nImplement the widget end to end.\n\n\
```mermaid\nflowchart TD\n    A[Start] --> B[Build]\n    B --> C[Done]\n```\n\n\
- Step 1: scaffold\n- Step 2: wire it up\n";
        let _ = self.invoke_mcp(
            "propose_plan",
            json!({ "title": "Widget plan", "markdown": markdown }),
        );
        self.emit_text("Plan saved via propose_plan.")
    }

    fn doc_review(&mut self) -> Result<(), String> {
        if self.message.contains("[mock:ask") {
            let quote = self
                .call_mcp_tool("get_review_doc", json!({}))?
                .and_then(|d| {
                    d["open_comments"]
                        .as_array()
                        .and_then(|cs| cs.first())
                        .and_then(|c| c["quote"].as_str())
                        .map(str::to_string)
                })
                .unwrap_or_default();
            if !self.tick()? {
                return Ok(());
            }
            self.emit_text("That passage reads two ways — asking before I guess.")?;
            if !self.tick()? {
                return Ok(());
            }
            let question = if quote.is_empty() {
                "Which reading of that passage did you mean?".to_string()
            } else {
                format!("Which reading of «{quote}» did you mean?")
            };
            let payload = if self.message.contains("[mock:ask:free]") {
                json!({ "questions": [{ "question": question, "header": "Intent" }] })
            } else if self.message.contains("[mock:ask:multi]") {
                json!({
                    "questions": [{
                        "question": question,
                        "header": "Intent",
                        "multiSelect": true,
                        "options": [
                            { "label": "Tighten the wording", "description": "Same meaning, fewer words." },
                            { "label": "Add an example", "description": "Show what it looks like in practice." },
                            { "label": "Split it in two", "description": "One idea per sentence." },
                            { "label": "Other", "description": "" }
                        ]
                    }]
                })
            } else {
                json!({
                    "questions": [{
                        "question": question,
                        "header": "Intent",
                        "options": [
                            { "label": "Keep it as written", "description": "Leave the passage alone." },
                            { "label": "Rewrite it", "description": "Replace it with clearer wording." },
                            { "label": "Other", "description": "" }
                        ]
                    }]
                })
            };
            self.call_mcp_tool("ask_user", payload)?;
            return self.completed(Value::Null);
        }

        if self.message.contains("[mock:chat]") {
            for chunk in [
                "Answering in the lane \u{2014} the document is unchanged.\n\n",
                "Three things worth knowing:\n\n1. The rotation lives in `oncall.yaml`.\n2. Escalation is a separate policy.\n3. Nothing here touches the document.\n\n",
                "| Field | Value |\n| --- | --- |\n| Owner | platform |\n| Cadence | quarterly |\n\n```bash\nmake verify\n```\n",
            ] {
                self.emit_text(chunk)?;
                if !self.tick()? {
                    return Ok(());
                }
            }
            return self.completed(Value::Null);
        }

        if self.message.contains("[mock:block]") {
            self.emit_text("Reading the document closely…")?;
            if !self.tick()? {
                return Ok(());
            }
            let tool_id = self.tool_id();
            self.tool_start(&tool_id, "mcp__peckboard__get_review_doc", json!({}))?;
            return self.wait_until_stop();
        }

        if let Some(doc) = self.call_mcp_tool("get_review_doc", json!({}))? {
            let markdown = doc["markdown"].as_str().unwrap_or_default().to_string();
            let version = doc["version"].as_i64().unwrap_or(1);
            let next = version + 1;
            let resolutions: Vec<Value> =
                doc["open_comments"]
                    .as_array()
                    .map(|cs| {
                        cs.iter()
                        .filter_map(|c| {
                            let id = c["id"].as_str()?;
                            let kind = c["kind"].as_str().unwrap_or("comment");
                            // A plain comment is a remark to answer; every
                            // other kind asks for a change to the text.
                            let action = if kind == "comment" { "answered" } else { "fixed" };
                            Some(json!({
                                "comment_id": id,
                                "action": action,
                                "note": format!("mock reviewer: {kind} {action} in pass {next}"),
                            }))
                        })
                        .collect()
                    })
                    .unwrap_or_default();
            if !self.tick()? {
                return Ok(());
            }
            self.call_mcp_tool(
                "submit_review_revision",
                json!({
                    "markdown": mock_revised_markdown(
                        &markdown,
                        next,
                        self.message.contains("[mock:insert]"),
                    ),
                    "note": format!("mock pass {next}"),
                    "resolutions": resolutions,
                }),
            )?;
            if !self.tick()? {
                return Ok(());
            }
            self.emit_text(&format!("Revised the document to v{next}."))?;
        }
        self.completed(Value::Null)
    }

    fn todo(&mut self) -> Result<(), String> {
        let raw_input = json!({
            "todos": [
                { "content": "Write the parser", "status": "completed", "activeForm": "Writing the parser" },
                { "content": "Wire up the route", "status": "in_progress", "activeForm": "Wiring up the route" },
                { "content": "Add tests", "status": "pending", "activeForm": "Adding tests" },
            ]
        });
        let tool_id = self.tool_id();
        self.tool_start(&tool_id, "TodoWrite", raw_input)?;
        if !self.tick()? {
            return Ok(());
        }
        self.tool_end_ok(&tool_id, "Todos updated")?;
        if !self.tick()? {
            return Ok(());
        }
        // snapshot_from_tool_call maps completed → done.
        self.emit_todo(json!([
            { "content": "Write the parser", "status": "done", "activeForm": "Writing the parser" },
            { "content": "Wire up the route", "status": "in_progress", "activeForm": "Wiring up the route" },
            { "content": "Add tests", "status": "pending", "activeForm": "Adding tests" },
        ]))
    }

    fn tasks(&mut self) -> Result<(), String> {
        let script: [(&str, Value, &str, Value); 6] = [
            (
                "TaskCreate",
                json!({
                    "subject": "Write the parser",
                    "description": "Parse the stream",
                    "activeForm": "Writing the parser",
                }),
                "Task #1 created successfully: Write the parser",
                json!({ "task": { "id": "1", "subject": "Write the parser" } }),
            ),
            (
                "TaskCreate",
                json!({
                    "subject": "Wire up the route",
                    "description": "Expose it over HTTP",
                    "activeForm": "Wiring up the route",
                }),
                "Task #2 created successfully: Wire up the route",
                json!({ "task": { "id": "2", "subject": "Wire up the route" } }),
            ),
            (
                "TaskCreate",
                json!({
                    "subject": "Add tests",
                    "description": "Lock in behaviour",
                    "activeForm": "Adding tests",
                }),
                "Task #3 created successfully: Add tests",
                json!({ "task": { "id": "3", "subject": "Add tests" } }),
            ),
            (
                "TaskUpdate",
                json!({ "taskId": "1", "status": "in_progress" }),
                "Updated task #1 status",
                json!({
                    "success": true,
                    "taskId": "1",
                    "statusChange": { "from": "pending", "to": "in_progress" },
                }),
            ),
            (
                "TaskUpdate",
                json!({ "taskId": "1", "status": "completed" }),
                "Updated task #1 status",
                json!({
                    "success": true,
                    "taskId": "1",
                    "statusChange": { "from": "in_progress", "to": "completed" },
                }),
            ),
            (
                "TaskUpdate",
                json!({ "taskId": "2", "status": "in_progress" }),
                "Updated task #2 status",
                json!({
                    "success": true,
                    "taskId": "2",
                    "statusChange": { "from": "pending", "to": "in_progress" },
                }),
            ),
        ];
        let snapshots = [
            json!([
                { "content": "Write the parser", "status": "pending", "activeForm": "Writing the parser" },
            ]),
            json!([
                { "content": "Write the parser", "status": "pending", "activeForm": "Writing the parser" },
                { "content": "Wire up the route", "status": "pending", "activeForm": "Wiring up the route" },
            ]),
            json!([
                { "content": "Write the parser", "status": "pending", "activeForm": "Writing the parser" },
                { "content": "Wire up the route", "status": "pending", "activeForm": "Wiring up the route" },
                { "content": "Add tests", "status": "pending", "activeForm": "Adding tests" },
            ]),
            json!([
                { "content": "Write the parser", "status": "in_progress", "activeForm": "Writing the parser" },
                { "content": "Wire up the route", "status": "pending", "activeForm": "Wiring up the route" },
                { "content": "Add tests", "status": "pending", "activeForm": "Adding tests" },
            ]),
            json!([
                { "content": "Write the parser", "status": "done", "activeForm": "Writing the parser" },
                { "content": "Wire up the route", "status": "pending", "activeForm": "Wiring up the route" },
                { "content": "Add tests", "status": "pending", "activeForm": "Adding tests" },
            ]),
            json!([
                { "content": "Write the parser", "status": "done", "activeForm": "Writing the parser" },
                { "content": "Wire up the route", "status": "in_progress", "activeForm": "Wiring up the route" },
                { "content": "Add tests", "status": "pending", "activeForm": "Adding tests" },
            ]),
        ];
        for (i, (name, input, output, _result)) in script.into_iter().enumerate() {
            let tool_id = self.tool_id();
            self.tool_start(&tool_id, name, input)?;
            if !self.tick()? {
                return Ok(());
            }
            self.tool_end_ok(&tool_id, output)?;
            self.emit_todo(snapshots[i].clone())?;
            if !self.tick()? {
                return Ok(());
            }
        }
        Ok(())
    }

    fn ctx(&self) -> Result<(), String> {
        let ctx: i64 = self.message.trim().parse().unwrap_or(160_000);
        self.emit_text(&format!("context now {ctx}"))?;
        if !self.tick()? {
            return Ok(());
        }
        self.emit(json!({
            "kind": "usage",
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_read_tokens": 0,
            "cache_creation_tokens": 0,
            "total_tokens": 150,
            "context_tokens": ctx,
            "model": self.model,
            "turn_seq": null,
        }))
    }
}

fn mock_revised_markdown(markdown: &str, version: i64, insert_only: bool) -> String {
    let mut lines: Vec<String> = markdown.lines().map(str::to_string).collect();
    if insert_only {
        let at = lines
            .iter()
            .position(|l| l.trim_start().starts_with('#'))
            .map_or(0, |i| i + 1);
        lines.insert(at, String::new());
        lines.insert(at + 1, format!("_Mock reviewer opener, pass {version}._"));
    } else {
        if let Some(line) = lines
            .iter_mut()
            .find(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        {
            *line = format!("Revised: {}", line.trim());
        }
        lines.push(String::new());
        lines.push(format!("_Mock reviewer pass {version}._"));
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Every ```mcp fenced block in `message`, parsed as JSON. Blocks that
/// fail to parse are skipped — the scenario reports counts, and a
/// malformed block should not abort the ones that follow.
fn extract_mcp_blocks(message: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut rest = message;
    while let Some(start) = rest.find("```mcp") {
        let after = &rest[start + "```mcp".len()..];
        let Some(end) = after.find("```") else { break };
        if let Ok(v) = serde_json::from_str::<Value>(after[..end].trim()) {
            out.push(v);
        }
        rest = &after[end + 3..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::extract_mcp_blocks;

    #[test]
    fn extracts_multiple_blocks_and_skips_malformed() {
        let msg = "intro\n```mcp\n{\"tool\":\"a\",\"args\":{\"x\":1}}\n```\nmiddle\n```mcp\nnot json\n```\n```mcp\n{\"tool\":\"b\"}\n```\n";
        let blocks = extract_mcp_blocks(msg);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["tool"], "a");
        assert_eq!(blocks[1]["tool"], "b");
        assert!(extract_mcp_blocks("no blocks here").is_empty());
    }
}
