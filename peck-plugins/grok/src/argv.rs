use crate::mcp;

/// Safe fallback model for empty / legacy ids. Kept as `grok-4.5` because
/// that id is in every auth-scoped CLI catalog (OAuth, API key, unauth);
/// newer flagships (`grok-4.6`, `grok-4.7`) are OAuth-gated and would
/// hard-error on API-key turns.
pub const DEFAULT_MODEL: &str = "grok-4.5";

/// The default model grok 0.x shipped with; grok 1.0 removed it from the
/// catalog, and passing it via `--model` now hard-errors with "unknown model
/// id". Stored sessions and cards may still carry it, so it maps to the
/// current default instead of failing every turn.
const LEGACY_DEFAULT_MODEL: &str = "grok-build";

/// Map a stored model id (already stripped of `grok:` / `@account`) to one
/// the current CLI accepts.
pub fn effective_model(base_model: &str) -> String {
    if base_model.is_empty() || base_model == LEGACY_DEFAULT_MODEL {
        DEFAULT_MODEL.to_string()
    } else {
        base_model.to_string()
    }
}

pub fn grok_mcp_tool_name(name: &str) -> String {
    let rest = name.strip_prefix("mcp__").unwrap_or(name);
    if rest.contains("__") {
        rest.to_string()
    } else {
        format!("peckboard__{rest}")
    }
}

/// Grok built-ins that Peckboard MCP replaces. Tool IDs are grok's
/// `--disallowed-tools` names (not the TUI labels). Pre-hatcher sessions
/// also lose web/task — the MCP server already hard-gates writes.
pub fn grok_denied_builtins(is_pre_hatcher: bool) -> &'static str {
    if is_pre_hatcher {
        "read_file,search_replace,list_dir,run_terminal_cmd,grep,grep_search,\
         web_search,web_fetch,todo_write,task"
    } else {
        "read_file,search_replace,list_dir,run_terminal_cmd,grep,grep_search"
    }
}

pub fn build_cli_args(
    model: &str,
    prompt: &str,
    conversation_id: Option<&str>,
    effort: Option<&str>,
    system_prompt: &str,
    has_mcp: bool,
    extra_mcp_servers: &[String],
    extra_disallowed: &[String],
    is_pre_hatcher: bool,
) -> Vec<String> {
    let mut args = vec![
        format!("--single={prompt}"),
        "--output-format=streaming-json".to_string(),
        "--always-approve".to_string(),
        "--trust".to_string(),
    ];
    let model = model.strip_prefix("grok:").unwrap_or(model);
    let model = model.split('@').next().unwrap_or(model);
    let model = effective_model(model);
    args.push(format!("--model={model}"));
    if let Some(cid) = conversation_id.filter(|c| !c.is_empty()) {
        args.push(format!("--resume={cid}"));
    }
    if let Some(effort) = effort.map(str::trim).filter(|e| !e.is_empty()) {
        args.push(format!("--effort={effort}"));
    }
    args.push(format!("--system-prompt-override={system_prompt}"));
    if has_mcp {
        args.push("--allow=MCPTool(peckboard__*)".to_string());
        for server in extra_mcp_servers {
            if server != "peckboard" && mcp::is_safe_server_name(server) {
                let rule = format!("--allow=MCPTool({server}__*)");
                if !args.contains(&rule) {
                    args.push(rule);
                }
            }
        }
        args.push(format!(
            "--disallowed-tools={}",
            grok_denied_builtins(is_pre_hatcher)
        ));
        for t in extra_disallowed {
            args.push(format!("--deny=MCPTool({})", grok_mcp_tool_name(t)));
        }
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sessions stored before the grok 1.0 catalog change carry `grok-build`,
    /// which the CLI now rejects with "unknown model id"; it must map to the
    /// current default rather than fail every turn.
    #[test]
    fn legacy_grok_build_maps_to_current_default() {
        assert_eq!(effective_model("grok-build"), DEFAULT_MODEL);
        assert_eq!(effective_model(""), DEFAULT_MODEL);
        assert_eq!(effective_model("grok-4.5"), "grok-4.5");
    }

    #[test]
    fn build_args_strips_prefix_and_account_before_mapping() {
        let args = build_cli_args(
            "grok:grok-build@acct_1",
            "hi",
            None,
            None,
            "sys",
            false,
            &[],
            &[],
            false,
        );
        assert!(args.contains(&"--model=grok-4.5".to_string()));
    }

    /// The denied-builtins list must name grok's real tool ids, and
    /// pre-hatcher sessions additionally lose web/task.
    #[test]
    fn denied_builtins_are_grok_tool_ids() {
        assert_eq!(
            grok_denied_builtins(false),
            "read_file,search_replace,list_dir,run_terminal_cmd,grep,grep_search"
        );
        let pre = grok_denied_builtins(true);
        for t in ["web_search", "web_fetch", "todo_write", "task"] {
            assert!(pre.split(',').any(|x| x == t), "{t} missing from {pre}");
        }
        assert!(pre.starts_with(grok_denied_builtins(false)));
    }

    #[test]
    fn mcp_wiring_allowlists_and_denies() {
        let args = build_cli_args(
            "grok-4.5",
            "hi",
            None,
            None,
            "sys",
            true,
            &["linear".into()],
            &["mcp__github__delete_repo".into()],
            false,
        );
        assert!(args.contains(&"--allow=MCPTool(peckboard__*)".to_string()));
        assert!(args.contains(&"--allow=MCPTool(linear__*)".to_string()));
        assert!(args.contains(&format!(
            "--disallowed-tools={}",
            grok_denied_builtins(false)
        )));
        assert!(args.contains(&"--deny=MCPTool(github__delete_repo)".to_string()));
    }
}
