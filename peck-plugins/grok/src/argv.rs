use crate::mcp;

pub fn grok_mcp_tool_name(name: &str) -> String {
    let rest = name.strip_prefix("mcp__").unwrap_or(name);
    if rest.contains("__") {
        rest.to_string()
    } else {
        format!("peckboard__{rest}")
    }
}

pub fn grok_denied_builtins(is_pre_hatcher: bool) -> &'static str {
    if is_pre_hatcher {
        "bash,read_file,write_file,edit_file,list_files"
    } else {
        "bash,read_file,write_file,edit_file"
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
    if !model.is_empty() && model != "default" {
        args.push(format!("--model={model}"));
    }
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
