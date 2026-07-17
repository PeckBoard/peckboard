pub mod oauth;
pub mod plan_usage;
pub mod process;
pub mod provider;
pub mod token_refresh;

pub use provider::{ClaudeProvider, register_claude_provider};

use crate::provider::stream::{ModelInfo, SpawnConfig};

/// System prompt appended to every session to standardize how the agent
/// asks the user questions via the AskUserQuestion tool.
const PECKBOARD_SYSTEM_PROMPT: &str = r#"
# Asking the user questions

You run inside Peckboard, a remote web UI — no terminal. To ask the user anything, call `mcp__peckboard__ask_user` (the built-in AskUserQuestion does NOT work headless). Never ask in plain text — the UI cannot render it.

Input: `{"questions":[{question, header, multiSelect?, options?}]}`
- `question` (string, required): the question text.
- `header` (string, required): short category label ("Setup", "Confirm", "Input").
- `multiSelect` (bool, default false): false = radio (pick one), true = checkboxes (pick multiple).
- `options` (array of `{label, description}`): renders multiple choice; `description` is required ("" if nothing to add). OMIT `options` for a free-form text input.
- Exactly ONE question per call — the UI shows a single-question dialog. Multiple answers = sequential calls; wait for each answer before the next.

Example (single select):
```json
{"questions":[{"question":"Which database should I use?","header":"Setup","options":[{"label":"PostgreSQL","description":"Production-grade relational DB"},{"label":"SQLite","description":"Lightweight, file-based"},{"label":"Other","description":"I'll type my preference"}]}]}
```

Answers return as text: the selected label; multi-select labels joined with ", "; free-form = the typed text.

Rules: prefer multiple choice when the valid answers are known, and always include an "Other" option; keep questions short and actionable; wait for the response — never assume it.

# Proactive clarification

Before starting a task, ask (via `mcp__peckboard__ask_user`) when the request is ambiguous, critical details are missing (language/framework/naming), a trade-off needs the user's call, or scope is unclear. If a task is impossible or blocked: explain why, then ask with alternatives/workarounds or a yes/no to confirm a different approach. Prefer asking over assuming — the user is remote and cannot see what you see.

# Directory restrictions

You are restricted to the current working directory and its subdirectories. Never read, write, or access paths outside the project folder — such attempts are denied. All file paths must stay within the project root.
"#;

/// Discover available Claude models.
pub(crate) fn discover_models() -> Vec<ModelInfo> {
    let mut models = vec![
        ModelInfo {
            id: "claude-fable-5".into(),
            display_name: "Claude Fable 5".into(),
            capabilities: vec!["code".into(), "reasoning".into(), "vision".into()],
            tier: 4,
        },
        ModelInfo {
            id: "claude-opus-4-8".into(),
            display_name: "Claude Opus 4.8".into(),
            capabilities: vec!["code".into(), "reasoning".into(), "vision".into()],
            tier: 3,
        },
        ModelInfo {
            id: "claude-opus-4-7".into(),
            display_name: "Claude Opus 4.7".into(),
            capabilities: vec!["code".into(), "reasoning".into(), "vision".into()],
            tier: 3,
        },
        ModelInfo {
            id: "claude-opus-4-6".into(),
            display_name: "Claude Opus 4.6".into(),
            capabilities: vec!["code".into(), "reasoning".into(), "vision".into()],
            tier: 3,
        },
        ModelInfo {
            id: "claude-sonnet-4-6".into(),
            display_name: "Claude Sonnet 4.6".into(),
            capabilities: vec!["code".into(), "vision".into()],
            tier: 2,
        },
        ModelInfo {
            id: "claude-haiku-4-5".into(),
            display_name: "Claude Haiku 4.5".into(),
            capabilities: vec!["code".into()],
            tier: 1,
        },
    ];

    // Check for Bedrock ARNs in environment
    for (env_var, tier) in &[
        ("ANTHROPIC_DEFAULT_OPUS_MODEL", 3),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL", 2),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL", 1),
    ] {
        if let Ok(arn) = std::env::var(env_var) {
            if !models.iter().any(|m| m.id == arn) {
                models.push(ModelInfo {
                    id: arn.clone(),
                    display_name: format!("Bedrock: {}", arn.split('/').last().unwrap_or(&arn)),
                    capabilities: vec!["code".into()],
                    tier: *tier,
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
    // one such flag, so we fold several sources into it: the standing
    // Peckboard prompt, the shared working-style rules, any per-spawn suffix
    // (e.g. repeating tasks), and — last — any per-session custom prompt set
    // via set_session_system_prompt.
    //
    // A per-session custom prompt EXTENDS the standing Peckboard prompt: it is
    // appended after the base guidance and suffix rather than replacing them,
    // so the operator's text layers on top of (not instead of) Peckboard's
    // own instructions.
    let combined_system_prompt = {
        // The shared working-style rules live in one place
        // (crate::provider::WORKING_STYLE) so every provider ships the same
        // guidance; append them to the standing Peckboard prompt.
        let mut prompt = PECKBOARD_SYSTEM_PROMPT.to_string();
        prompt.push_str(crate::provider::WORKING_STYLE);
        if let Some(suffix) = config.system_prompt_suffix.as_deref()
            && !suffix.is_empty()
        {
            prompt.push('\n');
            prompt.push_str(suffix);
        }
        if let Some(custom) = config
            .system_prompt_override
            .as_deref()
            .filter(|p| !p.is_empty())
        {
            prompt.push('\n');
            prompt.push_str(custom);
        }
        prompt
    };

    // `--disallowedTools` is a hard denylist that overrides
    // `--dangerously-skip-permissions`, so it's the real enforcement point.
    // AskUserQuestion is always denied (it doesn't work headless).
    //
    // Claude's built-in whole-file tools (Read/Write/Edit/MultiEdit) are denied
    // whenever Peckboard's own file tools are available — all file access is
    // then forced through the native read_file (partial, line-windowed) and
    // edit_file (hash-guarded diff) MCP tools, which enforce project-folder
    // containment and size caps. Those tools are now core, always-on MCP tools
    // (moved out of the former common-tools plugin), so the built-ins are
    // denied whenever an MCP config is wired up.
    let has_file_tools = {
        let core = crate::service::mcp_server::tool_names();
        ["read_file", "edit_file"].iter().all(|needed| {
            core.iter().any(|t| t == needed)
                || config.extra_allowed_tools.iter().any(|t| t == needed)
        })
    };
    // Pre-hatcher research sessions get a much longer denylist: every
    // built-in that can mutate, execute, spawn subagents, or reach outside
    // the MCP server's read-only, project-contained tools. The MCP server
    // already hard-gates its own tools for these sessions
    // (`pre_hatcher_allowed_tool_names`), but built-ins bypass that
    // server-side check, and prompt-level read-only rules alone have been
    // ignored in practice (runaway pre-hatcher turns edited files, ran
    // releases, and hunted processes via Bash). ToolSearch stays available
    // — it only loads MCP tool schemas.
    let mut disallowed: String = if config.is_pre_hatcher {
        "AskUserQuestion,Read,Write,Edit,MultiEdit,NotebookEdit,Bash,BashOutput,KillShell,\
         Glob,Grep,Task,Agent,WebFetch,WebSearch,Skill,SlashCommand,ExitPlanMode,\
         EnterWorktree,ExitWorktree,TodoWrite"
            .to_string()
    } else if has_file_tools {
        "AskUserQuestion,Read,Write,Edit,MultiEdit".to_string()
    } else {
        "AskUserQuestion".to_string()
    };
    // Per-tool switches from Settings → MCP Servers: the injected mcp.json
    // can't carry per-tool state, so user-disabled external tools are
    // hard-denied here instead.
    for t in &config.extra_disallowed_tools {
        disallowed.push(',');
        disallowed.push_str(t);
    }

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

        // Pre-approve every tool the Peckboard MCP server exposes: the core
        // tools (single source of truth via `tool_names()`) plus any tools
        // contributed by active plugins (e.g. common-tools' read_file /
        // edit_file), threaded in through `SpawnConfig::extra_allowed_tools`.
        // In the default bypass mode this is advisory, but it keeps "prompt"
        // permission mode from stalling on a plugin file tool.
        // Pre-hatcher sessions advertise only their read-only allowlist
        // (which already includes the plugin's own `pre_hatch_result`
        // hand-off tool); everything else gets the full core + plugin set.
        let mut names: Vec<String> = if config.is_pre_hatcher {
            crate::service::mcp_server::pre_hatcher_allowed_tool_names()
                .iter()
                .map(|t| t.to_string())
                .collect()
        } else {
            crate::service::mcp_server::tool_names()
        };
        if !config.is_pre_hatcher {
            for t in &config.extra_allowed_tools {
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

    // Only worker sessions may compact automatically. Peckboard's own
    // machinery already enforces that (crate::handover::maybe_auto_compact
    // is worker-gated; the UI prompts interactive users to clear / compact /
    // continue instead), but the CLI ALSO auto-compacts on its own near the
    // context limit — silently, bypassing that rule — so it is switched off
    // for non-workers here. Workers keep the built-in behaviour as a
    // backstop below peckboard's own threshold recycle.
    if !config.is_worker {
        args.push(r#"--settings={"autoCompactEnabled":false}"#.to_string());
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
            extra_allowed_tools: Vec::new(),
            extra_disallowed_tools: Vec::new(),
            is_worker: false,
            is_pre_hatcher: false,
        }
    }

    #[test]
    fn test_build_cli_args_autocompact_gated_on_worker() {
        // Non-worker spawns disable the CLI's built-in auto-compaction —
        // only worker sessions may compact automatically.
        let args = build_cli_args(&default_spawn("claude-opus-4-8"), None);
        assert!(args.contains(&r#"--settings={"autoCompactEnabled":false}"#.to_string()));

        let worker = SpawnConfig {
            is_worker: true,
            ..default_spawn("claude-opus-4-8")
        };
        let args = build_cli_args(&worker, None);
        assert!(!args.iter().any(|a| a.starts_with("--settings")));
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
    fn test_build_cli_args_system_prompt_override_extends_base() {
        let mut config = default_spawn("claude-opus-4-8");
        // A suffix is set too, to prove the custom prompt layers on top of
        // both the base Peckboard prompt AND the suffix rather than replacing
        // them.
        config.system_prompt_suffix = Some("# Repeating Task Context".to_string());
        config.system_prompt_override = Some("You are a pirate. Only say arrr.".to_string());

        let args = build_cli_args(&config, None);
        let append = args
            .iter()
            .find(|a| a.starts_with("--append-system-prompt="))
            .expect("append-system-prompt flag present");
        // The custom prompt is appended — the base prompt and suffix survive.
        assert!(append.contains("Asking the user questions"));
        assert!(append.contains("Repeating Task Context"));
        assert!(append.contains("You are a pirate. Only say arrr."));
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
            extra_allowed_tools: Vec::new(),
            extra_disallowed_tools: Vec::new(),
            is_worker: false,
            is_pre_hatcher: false,
        };

        let args = build_cli_args(&config, Some("conv-123"));
        assert!(args.contains(&"--resume=conv-123".to_string()));
        assert!(args.contains(&"--effort=high".to_string()));
        assert!(args.contains(&"--mcp-config=/tmp/mcp.json".to_string()));
        assert!(args.contains(&"--permission-prompt-tool=stdio".to_string()));
    }

    #[test]
    fn test_build_cli_args_pre_hatcher_locks_down_builtins() {
        // A pre-hatcher research session must not get ANY built-in with side
        // effects — the denylist is the real enforcement point (it overrides
        // --dangerously-skip-permissions) — and its MCP allowlist is exactly
        // the read-only pre-hatcher set, not the full tool surface.
        let config = SpawnConfig {
            mcp_config_path: Some("/tmp/mcp.json".into()),
            is_pre_hatcher: true,
            ..default_spawn("claude-haiku-4-5")
        };
        let args = build_cli_args(&config, None);
        let disallowed = args
            .iter()
            .find(|a| a.starts_with("--disallowedTools="))
            .expect("disallowedTools present");
        for builtin in [
            "Bash",
            "BashOutput",
            "KillShell",
            "Write",
            "Edit",
            "MultiEdit",
            "NotebookEdit",
            "Read",
            "Glob",
            "Grep",
            "Task",
            "Agent",
            "WebFetch",
            "WebSearch",
            "Skill",
            "SlashCommand",
        ] {
            assert!(
                disallowed
                    .trim_start_matches("--disallowedTools=")
                    .split(',')
                    .any(|t| t == builtin),
                "{builtin} must be denied for a pre-hatcher session"
            );
        }
        let allowed = args
            .iter()
            .find(|a| a.starts_with("--allowedTools="))
            .expect("allowedTools present");
        let names: Vec<&str> = allowed
            .trim_start_matches("--allowedTools=")
            .split(',')
            .collect();
        assert_eq!(
            names.len(),
            crate::service::mcp_server::pre_hatcher_allowed_tool_names().len(),
            "pre-hatcher allowlist should be exactly the read-only set"
        );
        assert!(names.contains(&"mcp__peckboard__read_file"));
        assert!(names.contains(&"mcp__peckboard__pre_hatch_result"));
        assert!(!names.iter().any(|n| n.contains("edit_file")));
    }
    #[test]
    fn test_build_cli_args_plain_session_keeps_full_toolset() {
        let config = SpawnConfig {
            mcp_config_path: Some("/tmp/mcp.json".into()),
            ..default_spawn("claude-opus-4-8")
        };
        let args = build_cli_args(&config, None);
        // read_file / edit_file are now core, always-on MCP tools, so Claude's
        // built-in whole-file tools are denied even for a plain session — all
        // file access routes through the containment-enforcing MCP tools.
        assert!(
            args.contains(
                &"--disallowedTools=AskUserQuestion,Read,Write,Edit,MultiEdit".to_string()
            )
        );
        let allowed = args
            .iter()
            .find(|a| a.starts_with("--allowedTools="))
            .expect("allowedTools present");
        assert!(allowed.contains("mcp__peckboard__create_card"));
        assert!(allowed.contains("mcp__peckboard__fetch_url"));
        // The allowlist is derived from the common MCP tool source, not a
        // hand-maintained Claude copy: every exposed tool is present, including
        // ones the old hardcoded list had drifted past (e.g. repeating tasks).
        assert!(allowed.contains("mcp__peckboard__create_repeating_task"));
        // With no plugin tools passed, the allowlist is exactly the core set.
        assert_eq!(
            allowed
                .trim_start_matches("--allowedTools=")
                .split(',')
                .count(),
            crate::service::mcp_server::tool_names().len(),
            "allowlist should list exactly the common tool set"
        );
    }

    #[test]
    fn user_disabled_mcp_tools_join_the_denylist() {
        let mut config = default_spawn("default");
        config.extra_disallowed_tools = vec![
            "mcp__gh__create_issue".to_string(),
            "mcp__gh__merge_pr".to_string(),
        ];
        let args = build_cli_args(&config, None);
        let disallowed = args
            .iter()
            .find(|a| a.starts_with("--disallowedTools="))
            .expect("disallowedTools present");
        let list = disallowed.trim_start_matches("--disallowedTools=");
        // The built-in denylist stays; the user's per-tool switches append.
        assert!(list.split(',').any(|t| t == "AskUserQuestion"));
        assert!(list.split(',').any(|t| t == "mcp__gh__create_issue"));
        assert!(list.split(',').any(|t| t == "mcp__gh__merge_pr"));
    }
    #[test]
    fn test_build_cli_args_merges_plugin_tools_and_denies_builtin_file_tools() {
        // Plugin-contributed tools are threaded in via extra_allowed_tools and
        // must appear in --allowedTools, while Claude's built-in Read/Write/Edit
        // are denied so agents route through the MCP file tools (now core).
        let config = SpawnConfig {
            mcp_config_path: Some("/tmp/mcp.json".into()),
            extra_allowed_tools: vec![
                "some_plugin_tool_a".into(),
                "some_plugin_tool_b".into(),
                // A duplicate of a core tool name must not double-list.
                "list_models".into(),
            ],
            ..default_spawn("claude-opus-4-8")
        };
        let args = build_cli_args(&config, None);

        let disallowed = args
            .iter()
            .find(|a| a.starts_with("--disallowedTools="))
            .expect("disallowedTools present");
        for builtin in ["Read", "Write", "Edit"] {
            assert!(
                disallowed.split(',').any(|t| t == builtin),
                "{builtin} should be denied"
            );
        }

        let allowed = args
            .iter()
            .find(|a| a.starts_with("--allowedTools="))
            .expect("allowedTools present");
        // read_file / edit_file are now core MCP tools (moved out of the plugin).
        assert!(allowed.contains("mcp__peckboard__read_file"));
        assert!(allowed.contains("mcp__peckboard__edit_file"));
        // The two genuinely-new plugin tools are threaded through.
        assert!(allowed.contains("mcp__peckboard__some_plugin_tool_a"));
        assert!(allowed.contains("mcp__peckboard__some_plugin_tool_b"));
        // list_models is a core tool; passing it again as a plugin name must
        // not produce a duplicate entry.
        let list_models_hits = allowed
            .trim_start_matches("--allowedTools=")
            .split(',')
            .filter(|t| *t == "mcp__peckboard__list_models")
            .count();
        assert_eq!(list_models_hits, 1, "core/plugin name overlap must dedupe");
        // Two genuinely new plugin tools beyond the core set.
        assert_eq!(
            allowed
                .trim_start_matches("--allowedTools=")
                .split(',')
                .count(),
            crate::service::mcp_server::tool_names().len() + 2,
        );
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
