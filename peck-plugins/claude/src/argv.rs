//! Claude CLI argv + stdin user-frame construction.

use serde_json::json;

const PECKBOARD_SYSTEM_PROMPT: &str = r#"
# Asking the user questions

You run inside Peckboard, a remote web UI — no terminal. To ask the user anything, call `mcp__peckboard__ask_user` (the built-in AskUserQuestion does NOT work headless). Never ask in plain text — the UI cannot render it.

Input: `{"questions":[{question, header, multiSelect?, options?}]}`
- `question` (string, required): the question text.
- `header` (string, required): short category label ("Setup", "Confirm", "Input").
- `multiSelect` (bool, default false): false = radio (pick one), true = checkboxes (pick multiple).
- `options` (array of `{label, description}`): renders multiple choice; `description` is required ("" if nothing to add). OMIT `options` for a free-form text input.
- Exactly ONE question per call — the UI shows a single-question dialog. Multiple answers = sequential calls; wait for each answer before the next.

Answers return as text: the selected label; multi-select labels joined with ", "; free-form = the typed text.

Rules: prefer multiple choice when the valid answers are known, and always include an "Other" option; keep questions short and actionable; wait for the response — never assume it.

# Proactive clarification

Before starting a task, ask (via `mcp__peckboard__ask_user`) when the request is ambiguous, critical details are missing (language/framework/naming), a trade-off needs the user's call, or scope is unclear. If a task is impossible or blocked: explain why, then ask with alternatives/workarounds or a yes/no to confirm a different approach. Prefer asking over assuming — the user is remote and cannot see what you see.

# Directory restrictions

You are restricted to the current working directory and its subdirectories. Never read, write, or access paths outside the project folder — such attempts are denied. All file paths must stay within the project root.
"#;

pub const SUBAGENT_CONTEXT: &str = r#"# Peckboard subagent rules

You are a subagent inside Peckboard, a remote web UI. There is no terminal, and the user does not see your output — only your final message returns to the agent that spawned you.

- NEVER use the terminal or shell tools (Bash and similar). To run a command, use the `mcp__peckboard__run_command` tool; use `mcp__peckboard__run_tests` for test suites and `mcp__peckboard__git` for git operations.
- Prefer the Peckboard code tools — `file_outline`, `read_symbol`, `search_files`, `read_file`, `edit_file` — to navigate and edit code. NEVER use `grep` or `sed` — not in commands, not in scripts; use `search_files` (ripgrep-backed) instead.
- The built-in Read/Write/Edit file tools may be disabled; use `mcp__peckboard__read_file`, `mcp__peckboard__edit_file`, and `mcp__peckboard__write_file` for file access.
- Stay inside the current working directory and its subdirectories — paths outside the project folder are denied.
- Never ask the user questions — no `ask_user`, no questions in plain text. If you are blocked or something is ambiguous, state the open question in your final message so the caller can decide.
"#;

/// SubagentStart hook context file, relative to the session folder. The
/// host's `write_file` is session-folder scoped, so unlike 0.1.11 (which
/// kept it next to the per-session MCP configs in the data dir) the file
/// lives under a dotted `.peckboard/` directory inside the working tree.
pub const HOOK_CONTEXT_FILE: &str = ".peckboard/claude-subagent-context.json";

/// The hook-output JSON the SubagentStart hook command `cat`s. The CLI only
/// honors the `hookSpecificOutput.additionalContext` envelope (0.1.11's
/// `write_subagent_context_file`); a bare `{"additionalContext": …}` object
/// is silently ignored.
pub fn subagent_context_json() -> String {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "SubagentStart",
            "additionalContext": SUBAGENT_CONTEXT,
        }
    })
    .to_string()
}

pub struct CliSpec {
    pub model: String,
    pub effort: Option<String>,
    pub conversation_id: Option<String>,
    pub mcp_config_path: Option<String>,
    pub permission_mode: Option<String>,
    pub is_worker: bool,
    pub is_pre_hatcher: bool,
    pub extra_allowed_tools: Vec<String>,
    pub extra_disallowed_tools: Vec<String>,
    pub system_prompt: String,
    pub core_tools: Vec<String>,
    pub pre_hatcher_tools: Vec<String>,
    pub subagent_context_path: Option<String>,
}

pub fn build_cli_args(spec: &CliSpec) -> Vec<String> {
    let combined_system_prompt = {
        let mut prompt = PECKBOARD_SYSTEM_PROMPT.to_string();
        if !spec.system_prompt.is_empty() {
            prompt.push_str(&spec.system_prompt);
        }
        prompt
    };

    let has_file_tools = ["read_file", "edit_file"].iter().all(|needed| {
        spec.core_tools.iter().any(|t| t == needed)
            || spec.extra_allowed_tools.iter().any(|t| t == needed)
    });
    let mut disallowed: String = if spec.is_pre_hatcher {
        "AskUserQuestion,Read,Write,Edit,MultiEdit,NotebookEdit,Bash,BashOutput,KillShell,\
         Glob,Grep,Task,Agent,WebFetch,WebSearch,Skill,SlashCommand,ExitPlanMode,\
         EnterWorktree,ExitWorktree,TodoWrite"
            .to_string()
    } else if has_file_tools {
        "AskUserQuestion,Read,Write,Edit,MultiEdit".to_string()
    } else {
        "AskUserQuestion".to_string()
    };
    for t in &spec.extra_disallowed_tools {
        disallowed.push(',');
        disallowed.push_str(t);
    }

    let mut args = vec![
        "--input-format=stream-json".to_string(),
        "--output-format=stream-json".to_string(),
        "--verbose".to_string(),
        "--include-partial-messages".to_string(),
        format!("--append-system-prompt={combined_system_prompt}"),
        format!("--disallowedTools={disallowed}"),
    ];

    if spec.model != "default" {
        let model = spec
            .model
            .strip_prefix("claude:")
            .unwrap_or(&spec.model)
            .split('@')
            .next()
            .unwrap_or(&spec.model);
        if !model.is_empty() && !model.starts_with('-') {
            args.push(format!("--model={model}"));
        }
    }
    if let Some(effort) = spec
        .effort
        .as_deref()
        .filter(|e| !e.is_empty() && !e.starts_with('-'))
    {
        args.push(format!("--effort={effort}"));
    }
    if let Some(cid) = spec
        .conversation_id
        .as_deref()
        .filter(|c| !c.is_empty() && !c.starts_with('-'))
    {
        args.push(format!("--resume={cid}"));
    }
    args.push("--strict-mcp-config".to_string());

    if let Some(mcp_path) = spec
        .mcp_config_path
        .as_deref()
        .filter(|p| !p.is_empty() && !p.starts_with('-'))
    {
        args.push(format!("--mcp-config={mcp_path}"));
        let mut names: Vec<String> = if spec.is_pre_hatcher {
            spec.pre_hatcher_tools.clone()
        } else {
            spec.core_tools.clone()
        };
        if !spec.is_pre_hatcher {
            for t in &spec.extra_allowed_tools {
                if !names.contains(t) {
                    names.push(t.clone());
                }
            }
        }
        let allowed: Vec<String> = names
            .iter()
            .map(|t| format!("mcp__peckboard__{t}"))
            .collect();
        args.push(format!("--allowedTools={}", allowed.join(",")));
    }

    let mut settings = serde_json::Map::new();
    if !spec.is_worker {
        settings.insert("autoCompactEnabled".into(), serde_json::Value::Bool(false));
    }
    if let Some(ctx) = spec.subagent_context_path.as_deref() {
        settings.insert(
            "hooks".into(),
            json!({
                "SubagentStart": [{
                    "hooks": [{
                        "type": "command",
                        "command": format!("cat '{ctx}'"),
                    }]
                }]
            }),
        );
    }
    if !settings.is_empty() {
        args.push(format!(
            "--settings={}",
            serde_json::Value::Object(settings)
        ));
    }
    match spec.permission_mode.as_deref() {
        Some("bypass") => args.push("--dangerously-skip-permissions".to_string()),
        _ => args.push("--permission-prompt-tool=stdio".to_string()),
    }
    args
}

pub fn build_user_message_frame(message: &serde_json::Value) -> String {
    let text = message.get("text").and_then(|v| v.as_str()).unwrap_or("");
    let attachments = message
        .get("attachments")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if attachments.is_empty() {
        return json!({
            "type": "user",
            "message": { "role": "user", "content": text },
        })
        .to_string();
    }
    let mut blocks: Vec<serde_json::Value> = Vec::new();
    for att in &attachments {
        let mime = att.get("mime_type").and_then(|v| v.as_str()).unwrap_or("");
        let data = att
            .get("data_base64")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if mime.starts_with("image/") && !data.is_empty() {
            blocks.push(json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": mime,
                    "data": data,
                },
            }));
        }
    }
    blocks.push(json!({ "type": "text", "text": text }));
    json!({
        "type": "user",
        "message": { "role": "user", "content": blocks },
    })
    .to_string()
}
