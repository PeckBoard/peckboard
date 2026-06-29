pub mod oauth;
pub mod process;
pub mod provider;

pub use provider::{ClaudeProvider, register_claude_provider};

use crate::provider::stream::{ModelInfo, SpawnConfig};

/// System prompt appended to every session to standardize how the agent
/// asks the user questions via the AskUserQuestion tool.
const PECKBOARD_SYSTEM_PROMPT: &str = r#"
# Asking the user questions

You are running inside Peckboard, a remote control panel. The user interacts through a web UI, not a terminal. When you need input from the user, you MUST use the `mcp__peckboard__ask_user` tool (NOT the built-in AskUserQuestion — that does not work in headless mode). Never ask questions in plain text — the UI cannot render interactive controls from plain text.

## Question format (JSON input to mcp__peckboard__ask_user)

Your input must be a JSON object with a `questions` array. Each question object has:

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `question` | string | yes | The question text displayed to the user |
| `header` | string | yes | Category label shown above the question. ALWAYS include this field — use a short category like "Setup", "Configuration", "Input", etc. |
| `multiSelect` | boolean | no | `false` = radio buttons (pick one), `true` = checkboxes (pick multiple). Default: `false` |
| `options` | array | no | If provided: renders as multiple choice. If omitted: renders as a text input (fill-in-the-blank) |

Each option in the `options` array is an object:

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `label` | string | yes | The option text the user sees and selects |
| `description` | string | yes | Help text shown below the label. ALWAYS include this — use a brief clarification or empty string "" if no extra detail is needed |

## IMPORTANT: One question per call

You MUST send exactly ONE question per `mcp__peckboard__ask_user` call. The UI shows questions one at a time as a dialog. If you have multiple questions, call the tool once for each question separately. Wait for the answer before asking the next question.

## Question types

**Multiple choice (single select)** — user picks exactly one option:
```json
{
  "questions": [{
    "question": "Which database should I use?",
    "header": "Setup",
    "options": [
      {"label": "PostgreSQL", "description": "Production-grade relational DB"},
      {"label": "SQLite", "description": "Lightweight, file-based"},
      {"label": "MySQL", "description": "Popular open-source relational DB"},
      {"label": "Other", "description": "I'll type my preference"}
    ]
  }]
}
```

**Multiple choice (multi select)** — user picks one or more options:
```json
{
  "questions": [{
    "question": "Which features should I include?",
    "header": "Features",
    "multiSelect": true,
    "options": [
      {"label": "Authentication", "description": "User login and registration"},
      {"label": "API rate limiting", "description": "Throttle excessive requests"},
      {"label": "WebSocket support", "description": "Real-time bidirectional communication"},
      {"label": "File uploads", "description": "Allow users to upload files"}
    ]
  }]
}
```

**Fill-in-the-blank** — user types a free-form answer:
```json
{
  "questions": [{
    "question": "What should the project be called?",
    "header": "Input"
  }]
}
```

**Yes/No confirmation:**
```json
{
  "questions": [{
    "question": "The file already exists. Should I overwrite it?",
    "header": "Confirm",
    "options": [
      {"label": "Yes", "description": "Overwrite the existing file"},
      {"label": "No", "description": "Keep the existing file"}
    ]
  }]
}
```

## Answer format (what you receive back)

After the user submits, you receive the answer as text:
- Single select: the selected label string
- Multi select: selected labels joined with ", "
- Fill-in-the-blank: the typed text

## Guidelines

- ALWAYS use `mcp__peckboard__ask_user` — never ask questions in plain text
- ONE question per call — the UI shows a single-question dialog
- If you need multiple answers, call the tool multiple times sequentially
- Use `description` when the option label alone isn't self-explanatory
- Prefer multiple choice over free-form when there is a known set of valid answers
- For multiple choice, always include an "Other" option so the user can provide a custom answer
- Keep questions concise and actionable
- Wait for the user's response before proceeding — do not assume answers

# Proactive clarification

Before starting any task, you MUST ask the user for clarification or context if:
- The request is ambiguous or could be interpreted multiple ways
- Critical details are missing (language, framework, architecture, naming, etc.)
- There are important trade-offs the user should decide on
- The task scope is unclear (how much to implement, what to include/exclude)

If a task is impossible or blocked, do NOT silently fail or guess. Instead:
1. Explain why the task cannot be completed
2. Use `mcp__peckboard__ask_user` to present options: possible alternatives, workarounds, or a yes/no to confirm whether to proceed with a different approach

Use yes/no questions for simple confirmations:
```json
{
  "questions": [{
    "question": "The file already exists. Should I overwrite it?",
    "header": "Confirm",
    "options": [
      {"label": "Yes", "description": "Overwrite the existing file"},
      {"label": "No", "description": "Keep the existing file and skip"}
    ]
  }]
}
```

Always prefer asking over assuming. The user is remote and cannot see what you see — keep them informed and in control.

# Directory restrictions

You are restricted to the current working directory and its subdirectories. Do NOT read, write, edit, or access any files or directories outside of this project folder. Any attempt to access paths outside the project directory will be denied. All file paths must be within the project root.
"#;

/// Discover available Claude models.
pub(crate) fn discover_models() -> Vec<ModelInfo> {
    let mut models = vec![
        ModelInfo {
            id: "claude-fable-5".into(),
            display_name: "Claude Fable 5".into(),
            capabilities: vec!["code".into(), "reasoning".into(), "vision".into()],
        },
        ModelInfo {
            id: "claude-opus-4-8".into(),
            display_name: "Claude Opus 4.8".into(),
            capabilities: vec!["code".into(), "reasoning".into(), "vision".into()],
        },
        ModelInfo {
            id: "claude-opus-4-7".into(),
            display_name: "Claude Opus 4.7".into(),
            capabilities: vec!["code".into(), "reasoning".into(), "vision".into()],
        },
        ModelInfo {
            id: "claude-opus-4-6".into(),
            display_name: "Claude Opus 4.6".into(),
            capabilities: vec!["code".into(), "reasoning".into(), "vision".into()],
        },
        ModelInfo {
            id: "claude-sonnet-4-6".into(),
            display_name: "Claude Sonnet 4.6".into(),
            capabilities: vec!["code".into(), "vision".into()],
        },
        ModelInfo {
            id: "claude-haiku-4-5".into(),
            display_name: "Claude Haiku 4.5".into(),
            capabilities: vec!["code".into()],
        },
    ];

    // Check for Bedrock ARNs in environment
    for env_var in &[
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    ] {
        if let Ok(arn) = std::env::var(env_var) {
            if !models.iter().any(|m| m.id == arn) {
                models.push(ModelInfo {
                    id: arn.clone(),
                    display_name: format!("Bedrock: {}", arn.split('/').last().unwrap_or(&arn)),
                    capabilities: vec!["code".into()],
                });
            }
        }
    }

    models
}

/// Build the CLI arguments for spawning a long-lived Claude process.
///
/// # Stream-json mode
///
/// The CLI is spawned in `--input-format stream-json --output-format
/// stream-json` mode. User messages are NOT in argv — they're written
/// to stdin as JSON envelopes (`{"type":"user", "message":{...}}`) one
/// per line. This lets a single child handle many turns and accept
/// new user messages mid-turn (the CLI queues them internally and
/// consumes them after the current `result`). See the long-form
/// architectural note on `process::ClaudeProcess`.
///
/// # Argument-injection hardening
///
/// Several values flowing into this function are user-controlled
/// (`config.model` and `config.effort` come straight from the HTTP
/// request body of an authenticated user; `conversation_id` is
/// extracted from the CLI's own output). The Claude CLI uses
/// commander.js, which parses `--flag value` by consuming the next
/// argv entry — and if that entry starts with `-`, commander treats it
/// as a separate option instead. An attacker who could pick the value
/// of one of these fields could therefore inject arbitrary flags into
/// the spawned `claude` process (`--mcp-config`, `--allowedTools`, …),
/// which would be a real escalation given the CLI runs with
/// `--dangerously-skip-permissions`.
///
/// Defence: every value-taking flag uses the `--name=VALUE` form,
/// which commander.js parses unambiguously regardless of what VALUE
/// starts with. The prompt no longer enters argv at all (it goes over
/// stdin), so there is no positional argument to worry about. Both
/// properties are exercised by the regression tests below.
pub fn build_cli_args(config: &SpawnConfig, conversation_id: Option<&str>) -> Vec<String> {
    // Build the single --append-system-prompt value. Claude's CLI takes only
    // one such flag, so we fold two sources into it: the standing Peckboard
    // prompt and any per-spawn suffix (e.g. repeating tasks).
    // A per-session custom prompt (set via set_session_system_prompt) FULLY
    // replaces the standing Peckboard prompt and any suffix — the operator
    // who set it owns the whole system prompt for this session.
    let combined_system_prompt = if let Some(override_prompt) = config
        .system_prompt_override
        .as_deref()
        .filter(|p| !p.is_empty())
    {
        override_prompt.to_string()
    } else {
        let mut prompt = PECKBOARD_SYSTEM_PROMPT.to_string();
        if let Some(suffix) = config.system_prompt_suffix.as_deref()
            && !suffix.is_empty()
        {
            prompt.push('\n');
            prompt.push_str(suffix);
        }
        prompt
    };

    // `--disallowedTools` is a hard denylist that overrides
    // `--dangerously-skip-permissions`, so it's the real enforcement point.
    // AskUserQuestion is denied for every session (it doesn't work headless).
    let disallowed = "AskUserQuestion";

    let mut args = vec![
        "claude".to_string(),
        "--input-format=stream-json".to_string(),
        "--output-format=stream-json".to_string(),
        "--verbose".to_string(),
        format!("--append-system-prompt={combined_system_prompt}"),
        format!("--disallowedTools={disallowed}"),
    ];

    if config.model != "default" {
        // Strip provider prefix if present (e.g. "claude:claude-opus-4-8" → "claude-opus-4-8")
        let model = config
            .model
            .strip_prefix("claude:")
            .unwrap_or(&config.model);
        args.push(format!("--model={model}"));
    }

    if let Some(effort) = &config.effort {
        args.push(format!("--effort={effort}"));
    }

    if let Some(conv_id) = conversation_id {
        args.push(format!("--resume={conv_id}"));
    }

    // Only use MCP servers we explicitly provide — skip built-in Anthropic
    // servers (Figma, Gmail, etc.) which add ~2s to CLI startup.
    args.push("--strict-mcp-config".to_string());

    if let Some(mcp_path) = &config.mcp_config_path {
        args.push(format!("--mcp-config={mcp_path}"));

        // Tell Claude which MCP tools are allowed.
        let full_mcp_tools = [
            "create_card",
            "list_projects",
            "list_workflows",
            "list_cards",
            "write_report",
            "attach_report_file",
            "update_card",
            "update_project",
            "create_project",
            "create_folder",
            "list_folders",
            "pause_project",
            "resume_project",
            "delete_project",
            "delete_card",
            "move_card_to_done",
            "move_card_to_wont_do",
            // Worker-only tools (harmless to allow for plain sessions — they just
            // fail with "requires card context" if called without one)
            "complete_step",
            "finish_card",
            "wont_do_card",
            "ask_user",
            "notify_workers",
            "fetch_url",
            "share_finding",
            "get_finding_details",
            "send_worker_message",
            "list_project_reports",
            "read_report",
            "read_worker_session",
            "list_worker_sessions",
            // Cross-session debug tools — available to every session
            // (chat included) for reading/grepping other sessions.
            "list_sessions",
            "search_sessions",
            "set_session_system_prompt",
            "upgrade_plugin",
            "list_models",
        ];
        let allowed: Vec<String> = full_mcp_tools
            .iter()
            .map(|t| format!("mcp__peckboard__{t}"))
            .collect();
        args.push(format!("--allowedTools={}", allowed.join(",")));
    }

    // Permission handling: Peckboard runs Claude headless, so we need
    // to skip interactive permission prompts. Without this, the CLI
    // blocks waiting for user input on tool use approvals.
    match config.permission_mode.as_deref() {
        Some("bypass") | None => {
            // Default for headless operation: skip all permission prompts
            args.push("--dangerously-skip-permissions".to_string());
        }
        Some("prompt") => {
            // Interactive mode: use stdio for permission prompts
            // (answers delivered via stdin channel)
            args.push("--permission-prompt-tool=stdio".to_string());
        }
        Some(_) => {
            // Unknown mode: default to skip
            args.push("--dangerously-skip-permissions".to_string());
        }
    }

    args
}

/// Build the JSON envelope for a user message sent over stdin in
/// stream-json mode. The CLI consumes one envelope per line and treats
/// each as a fresh user turn.
///
/// A message with no attachments uses the simple string-content form
/// (back-compat with the long-standing single-text-turn path). A
/// message with image attachments switches to the Anthropic content
/// blocks array, with one `image` block per attachment followed by a
/// `text` block — that's the only shape the CLI accepts for multimodal
/// turns. Non-image attachments are dropped with a warning rather than
/// silently corrupting the envelope: the Messages API rejects anything
/// other than `image` / `text` blocks in a user turn.
pub fn build_user_message_frame(msg: &crate::provider::message::UserMessage) -> String {
    use base64::Engine as _;

    if msg.attachments.is_empty() {
        return serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": msg.text },
        })
        .to_string();
    }

    let mut blocks: Vec<serde_json::Value> = Vec::with_capacity(msg.attachments.len() + 1);
    for att in &msg.attachments {
        if att.mime_type.starts_with("image/") {
            let encoded = base64::engine::general_purpose::STANDARD.encode(&att.data);
            blocks.push(serde_json::json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": att.mime_type,
                    "data": encoded,
                },
            }));
        } else {
            tracing::warn!(
                filename = %att.filename,
                mime = %att.mime_type,
                "Dropping non-image attachment — Anthropic user turns only accept image / text blocks"
            );
        }
    }

    // Text block follows the images so the model reads them as
    // "here's a picture, now my question." If text is empty we still
    // emit an empty text block — Anthropic accepts it, and dropping
    // the block entirely would leave an unusual "image only" turn
    // that some downstream tooling stumbles over.
    blocks.push(serde_json::json!({ "type": "text", "text": msg.text }));

    serde_json::json!({
        "type": "user",
        "message": { "role": "user", "content": blocks },
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discover_models() {
        let models = discover_models();
        assert!(models.len() >= 5);
        assert!(models.iter().any(|m| m.id == "claude-fable-5"));
        assert!(models.iter().any(|m| m.id == "claude-opus-4-8"));
        assert!(models.iter().any(|m| m.id == "claude-sonnet-4-6"));
        assert!(models.iter().any(|m| m.id == "claude-haiku-4-5"));
    }

    fn default_spawn(model: &str) -> SpawnConfig {
        SpawnConfig {
            model: model.into(),
            effort: None,
            working_dir: "/tmp".into(),
            mcp_config_path: None,
            env: Default::default(),
            permission_mode: None,
            timeout_ms: None,
            metadata: serde_json::Value::Null,
            system_prompt_suffix: None,
            system_prompt_override: None,
        }
    }

    /// Returns true iff `args` contains an entry equal to `value`. Used
    /// by the injection tests to assert that an attacker-supplied value
    /// is NEVER passed as a standalone argv entry (which is how
    /// commander.js would parse it as a separate flag).
    fn has_bare(args: &[String], value: &str) -> bool {
        args.iter().any(|a| a == value)
    }

    #[test]
    fn test_build_cli_args_includes_system_prompt_suffix() {
        let mut config = default_spawn("claude-opus-4-8");
        config.system_prompt_suffix = Some("# Repeating Task Context\n\nrun #42".to_string());

        let args = build_cli_args(&config, None);
        // The base prompt and the suffix are concatenated into the same
        // --append-system-prompt flag value (Claude's CLI takes only one).
        let append = args
            .iter()
            .find(|a| a.starts_with("--append-system-prompt="))
            .expect("append-system-prompt flag present");
        assert!(append.contains("Asking the user questions"));
        assert!(append.contains("Repeating Task Context"));
        assert!(append.contains("run #42"));
    }

    #[test]
    fn test_build_cli_args_system_prompt_override_fully_replaces() {
        let mut config = default_spawn("claude-opus-4-8");
        // A suffix is set too, to prove the override wins over both the base
        // Peckboard prompt AND the suffix.
        config.system_prompt_suffix = Some("# Repeating Task Context".to_string());
        config.system_prompt_override = Some("You are a pirate. Only say arrr.".to_string());

        let args = build_cli_args(&config, None);
        let append = args
            .iter()
            .find(|a| a.starts_with("--append-system-prompt="))
            .expect("append-system-prompt flag present");
        // The override IS the entire value — base prompt and suffix are gone.
        assert_eq!(
            append,
            "--append-system-prompt=You are a pirate. Only say arrr."
        );
        assert!(!append.contains("Asking the user questions"));
        assert!(!append.contains("Repeating Task Context"));
    }

    #[test]
    fn test_build_cli_args_empty_override_falls_back_to_base() {
        let mut config = default_spawn("claude-opus-4-8");
        config.system_prompt_override = Some(String::new());

        let args = build_cli_args(&config, None);
        let append = args
            .iter()
            .find(|a| a.starts_with("--append-system-prompt="))
            .expect("append-system-prompt flag present");
        // An empty override is treated as "unset" — the base prompt stands.
        assert!(append.contains("Asking the user questions"));
    }

    #[test]
    fn test_build_cli_args_omits_empty_suffix() {
        let mut config = default_spawn("claude-opus-4-8");
        config.system_prompt_suffix = Some(String::new());

        let with_empty = build_cli_args(&config, None);
        let none = default_spawn("claude-opus-4-8");
        let with_none = build_cli_args(&none, None);

        // Empty-string and None must produce byte-identical CLI args —
        // no "None" marker, no spurious trailing blank line from the
        // concatenation path.
        assert_eq!(with_empty, with_none);
    }

    #[test]
    fn test_build_cli_args_basic() {
        let config = default_spawn("claude-opus-4-8");

        let args = build_cli_args(&config, None);
        assert!(args.contains(&"claude".to_string()));
        // Stream-json mode in both directions — the prompt is no longer
        // in argv; it goes over stdin as `{type:'user', ...}` envelopes.
        assert!(args.contains(&"--input-format=stream-json".to_string()));
        assert!(args.contains(&"--output-format=stream-json".to_string()));
        assert!(args.contains(&"--model=claude-opus-4-8".to_string()));
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(!args.iter().any(|a| a.starts_with("--resume")));
        // No positional prompt — no `--` separator either.
        assert!(!args.contains(&"--".to_string()));
        // And no `-p` (one-shot print mode) since we run interactively.
        assert!(!args.contains(&"-p".to_string()));
    }

    #[test]
    fn test_build_cli_args_with_resume() {
        let config = SpawnConfig {
            model: "claude-sonnet-4-6".into(),
            effort: Some("high".into()),
            working_dir: "/tmp".into(),
            mcp_config_path: Some("/tmp/mcp.json".into()),
            env: Default::default(),
            permission_mode: Some("prompt".into()),
            timeout_ms: None,
            metadata: serde_json::Value::Null,
            system_prompt_suffix: None,
            system_prompt_override: None,
        };

        let args = build_cli_args(&config, Some("conv-123"));
        assert!(args.contains(&"--resume=conv-123".to_string()));
        assert!(args.contains(&"--effort=high".to_string()));
        assert!(args.contains(&"--mcp-config=/tmp/mcp.json".to_string()));
        assert!(args.contains(&"--permission-prompt-tool=stdio".to_string()));
    }

    #[test]
    fn test_build_cli_args_plain_session_keeps_full_toolset() {
        // A non-restricted session keeps the full MCP allowlist and only the
        // baseline AskUserQuestion denial.
        let config = SpawnConfig {
            mcp_config_path: Some("/tmp/mcp.json".into()),
            ..default_spawn("claude-opus-4-8")
        };
        let args = build_cli_args(&config, None);
        assert!(args.contains(&"--disallowedTools=AskUserQuestion".to_string()));
        let allowed = args
            .iter()
            .find(|a| a.starts_with("--allowedTools="))
            .expect("allowedTools present");
        assert!(allowed.contains("mcp__peckboard__create_card"));
        assert!(allowed.contains("mcp__peckboard__fetch_url"));
    }

    #[test]
    fn test_build_cli_args_default_model() {
        let config = default_spawn("default");

        let args = build_cli_args(&config, None);
        assert!(!args.iter().any(|a| a.starts_with("--model")));
    }

    #[test]
    fn test_user_message_frame_round_trips() {
        // The frame is what we write to the CLI's stdin to start (or
        // queue) a new turn. The CLI parses each line as a JSON object
        // with shape `{type:"user", message:{role,content}}`. With no
        // attachments, content stays a string for back-compat with
        // every prior dispatcher.
        use crate::provider::message::UserMessage;
        let frame = build_user_message_frame(&UserMessage::from_text("hello world"));
        let parsed: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(parsed["type"], "user");
        assert_eq!(parsed["message"]["role"], "user");
        assert_eq!(parsed["message"]["content"], "hello world");
    }

    #[test]
    fn test_user_message_frame_handles_control_chars() {
        // Newlines and quotes in the prompt must be JSON-escaped — they
        // can't break the one-line-per-envelope framing the CLI uses on
        // stdin. (No matter the prompt content, `serde_json::to_string`
        // will produce a single JSON line with embedded escapes.)
        use crate::provider::message::UserMessage;
        let frame = build_user_message_frame(&UserMessage::from_text(
            "line one\nline two with \"quotes\"",
        ));
        assert!(!frame.contains('\n'));
        let parsed: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(
            parsed["message"]["content"],
            "line one\nline two with \"quotes\""
        );
    }

    #[test]
    fn test_user_message_frame_emits_image_blocks() {
        // With image attachments, the frame switches to the content
        // blocks array shape: `[{image…}, {image…}, {text}]`. Each
        // image carries the base64-encoded bytes + media type the
        // Anthropic Messages API expects.
        use crate::provider::message::{UserAttachment, UserMessage};
        let msg = UserMessage {
            text: "what's in this picture?".into(),
            attachments: vec![UserAttachment {
                filename: "shot.png".into(),
                mime_type: "image/png".into(),
                data: vec![0xDE, 0xAD, 0xBE, 0xEF],
            }],
        };
        let frame = build_user_message_frame(&msg);
        let parsed: serde_json::Value = serde_json::from_str(&frame).unwrap();
        let content = parsed["message"]["content"]
            .as_array()
            .expect("content is an array when attachments are present");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[0]["source"]["type"], "base64");
        assert_eq!(content[0]["source"]["media_type"], "image/png");
        assert_eq!(content[0]["source"]["data"], "3q2+7w==");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "what's in this picture?");
    }

    #[test]
    fn test_user_message_frame_drops_non_image_attachments() {
        // The Messages API rejects unknown content block types in a
        // user turn, so a non-image attachment must not make it into
        // the envelope. The text block still goes through.
        use crate::provider::message::{UserAttachment, UserMessage};
        let msg = UserMessage {
            text: "see attached".into(),
            attachments: vec![UserAttachment {
                filename: "notes.txt".into(),
                mime_type: "text/plain".into(),
                data: b"hello".to_vec(),
            }],
        };
        let frame = build_user_message_frame(&msg);
        let parsed: serde_json::Value = serde_json::from_str(&frame).unwrap();
        let content = parsed["message"]["content"]
            .as_array()
            .expect("content is an array when attachments are present");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "see attached");
    }

    // ── Argument-injection regression tests ────────────────────────
    //
    // Each of these covers a field whose value is either directly
    // user-controlled (model, effort) or derived from data the CLI
    // itself produced (conversation_id, mcp_config_path). The tests
    // assert that even if the attacker hands us a value that LOOKS like
    // a CLI flag, the flag never appears as its own argv entry — it is
    // always fused to its parent via `=`, so commander.js parses it as
    // the option's value, not as a separate option.

    #[test]
    fn test_model_with_leading_dash_is_not_injected() {
        // Attacker scenario: POST /api/sessions/:id/message
        //   { "text": "...", "model": "--allowedTools=Bash" }
        // If we had passed `--model --allowedTools=Bash`, commander
        // would have consumed `--allowedTools=Bash` as its own option
        // and bypassed the tool allow-list.
        let config = default_spawn("--allowedTools=Bash");

        let args = build_cli_args(&config, None);

        assert!(args.contains(&"--model=--allowedTools=Bash".to_string()));
        assert!(
            !has_bare(&args, "--allowedTools=Bash"),
            "attacker value must NEVER appear as a standalone argv entry"
        );
        // And no bare `--model` either — that would be the smoking gun
        // that the attacker value was about to be split off.
        assert!(!has_bare(&args, "--model"));
    }

    #[test]
    fn test_effort_with_leading_dash_is_not_injected() {
        let mut config = default_spawn("claude-opus-4-8");
        config.effort = Some("--mcp-config=/tmp/evil.json".into());

        let args = build_cli_args(&config, None);

        assert!(args.contains(&"--effort=--mcp-config=/tmp/evil.json".to_string()));
        assert!(!has_bare(&args, "--mcp-config=/tmp/evil.json"));
        assert!(!has_bare(&args, "--effort"));
    }

    #[test]
    fn test_conversation_id_with_leading_dash_is_not_injected() {
        // conversation_id comes from the CLI's own `system init` event,
        // so an attacker would need to influence that stream — but
        // defence-in-depth: even a malformed value can't smuggle a
        // flag.
        let config = default_spawn("default");

        let args = build_cli_args(&config, Some("--allowedTools=Bash"));

        assert!(args.contains(&"--resume=--allowedTools=Bash".to_string()));
        assert!(!has_bare(&args, "--allowedTools=Bash"));
        assert!(!has_bare(&args, "--resume"));
    }

    #[test]
    fn test_mcp_config_path_with_leading_dash_is_not_injected() {
        // The mcp config path is server-controlled today (built from
        // the data dir), but a future caller could conceivably make it
        // reflect a project-scoped value. Harden now so that change
        // can't introduce a hole.
        let mut config = default_spawn("default");
        config.mcp_config_path = Some("--allowedTools=Bash".into());

        let args = build_cli_args(&config, None);

        assert!(args.contains(&"--mcp-config=--allowedTools=Bash".to_string()));
        // The allowedTools the build emits is the legitimate list of
        // mcp__peckboard__* tools — so make the "bare allowedTools"
        // assertion specific to the attacker value.
        assert!(!has_bare(&args, "--allowedTools=Bash"));
        assert!(!has_bare(&args, "--mcp-config"));
    }

    #[test]
    fn test_no_user_value_becomes_a_standalone_flag() {
        // Sanity sweep: combine every user-influenced field with a
        // flag-shaped value, then assert that NO argv entry in the
        // result equals one of those attacker values verbatim. This
        // catches a future regression where someone reintroduces the
        // two-arg form for any one of them.
        let mut config = default_spawn("--evil-model");
        config.effort = Some("--evil-effort".into());
        config.mcp_config_path = Some("--evil-mcp".into());

        let evil_values = ["--evil-model", "--evil-effort", "--evil-mcp", "--evil-conv"];
        let args = build_cli_args(&config, Some("--evil-conv"));

        for evil in evil_values {
            assert!(
                !has_bare(&args, evil),
                "attacker value {evil:?} appeared as a standalone argv entry: {args:?}"
            );
        }
    }
}
