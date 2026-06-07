pub mod process;
pub mod provider;

pub use provider::register_claude_provider;

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

/// Build the CLI arguments for spawning a Claude process.
pub fn build_cli_args(
    message: &str,
    config: &SpawnConfig,
    conversation_id: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "claude".to_string(),
        "-p".to_string(),
        message.to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
        "--append-system-prompt".to_string(),
        PECKBOARD_SYSTEM_PROMPT.to_string(),
        // Block the built-in AskUserQuestion tool — it doesn't work in headless
        // mode. The agent must use mcp__peckboard__ask_user instead.
        "--disallowedTools".to_string(),
        "AskUserQuestion".to_string(),
    ];

    if config.model != "default" {
        args.push("--model".to_string());
        // Strip provider prefix if present (e.g. "claude:claude-opus-4-8" → "claude-opus-4-8")
        let model = config
            .model
            .strip_prefix("claude:")
            .unwrap_or(&config.model);
        args.push(model.to_string());
    }

    if let Some(effort) = &config.effort {
        args.push("--effort".to_string());
        args.push(effort.clone());
    }

    if let Some(conv_id) = conversation_id {
        args.push("--resume".to_string());
        args.push(conv_id.to_string());
    }

    // Only use MCP servers we explicitly provide — skip built-in Anthropic
    // servers (Figma, Gmail, etc.) which add ~2s to CLI startup.
    args.push("--strict-mcp-config".to_string());

    if let Some(mcp_path) = &config.mcp_config_path {
        args.push("--mcp-config".to_string());
        args.push(mcp_path.clone());

        // Tell Claude which MCP tools are allowed
        let mcp_tools = [
            "create_card",
            "list_projects",
            "list_workflows",
            "list_cards",
            "write_report",
            "attach_report_file",
            "update_card",
            "update_project",
            "create_project",
            "pause_project",
            "resume_project",
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
        ];
        let allowed: Vec<String> = mcp_tools
            .iter()
            .map(|t| format!("mcp__peckboard__{t}"))
            .collect();
        args.push("--allowedTools".to_string());
        args.push(allowed.join(","));
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
            args.push("--permission-prompt-tool".to_string());
            args.push("stdio".to_string());
        }
        Some(_) => {
            // Unknown mode: default to skip
            args.push("--dangerously-skip-permissions".to_string());
        }
    }

    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discover_models() {
        let models = discover_models();
        assert!(models.len() >= 5);
        assert!(models.iter().any(|m| m.id == "claude-opus-4-8"));
        assert!(models.iter().any(|m| m.id == "claude-sonnet-4-6"));
        assert!(models.iter().any(|m| m.id == "claude-haiku-4-5"));
    }

    #[test]
    fn test_build_cli_args_basic() {
        let config = SpawnConfig {
            model: "claude-opus-4-8".into(),
            effort: None,
            working_dir: "/tmp".into(),
            mcp_config_path: None,
            env: Default::default(),
            permission_mode: None,
            timeout_ms: None,
            metadata: serde_json::Value::Null,
        };

        let args = build_cli_args("hello", &config, None);
        assert!(args.contains(&"claude".to_string()));
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"hello".to_string()));
        assert!(args.contains(&"--model".to_string()));
        assert!(args.contains(&"claude-opus-4-8".to_string()));
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(!args.contains(&"--resume".to_string()));
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
        };

        let args = build_cli_args("hi", &config, Some("conv-123"));
        assert!(args.contains(&"--resume".to_string()));
        assert!(args.contains(&"conv-123".to_string()));
        assert!(args.contains(&"--effort".to_string()));
        assert!(args.contains(&"high".to_string()));
        assert!(args.contains(&"--mcp-config".to_string()));
        assert!(args.contains(&"--permission-prompt-tool".to_string()));
    }

    #[test]
    fn test_build_cli_args_default_model() {
        let config = SpawnConfig {
            model: "default".into(),
            effort: None,
            working_dir: "/tmp".into(),
            mcp_config_path: None,
            env: Default::default(),
            permission_mode: None,
            timeout_ms: None,
            metadata: serde_json::Value::Null,
        };

        let args = build_cli_args("test", &config, None);
        assert!(!args.contains(&"--model".to_string()));
    }
}
