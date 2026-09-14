pub fn map_effort(effort: Option<&str>) -> Option<&str> {
    match effort.map(str::trim).filter(|e| !e.is_empty()) {
        Some("max") | Some("ultra") => Some("xhigh"),
        other => other,
    }
}

pub fn is_auto_model(model: &str) -> bool {
    matches!(model, "" | "default" | "auto")
}

pub fn build_cli_args(
    model: &str,
    prompt: &str,
    conversation_id: Option<&str>,
    effort: Option<&str>,
    system_prompt: &str,
    mcp_overrides: &[String],
) -> Vec<String> {
    let mut args = vec![
        "exec".into(),
        "--json".into(),
        "--dangerously-bypass-approvals-and-sandbox".into(),
        "--skip-git-repo-check".into(),
    ];
    for over in mcp_overrides {
        args.push("-c".into());
        args.push(over.clone());
    }
    let model = model.strip_prefix("codex:").unwrap_or(model);
    let model = model.split('@').next().unwrap_or(model);
    if !is_auto_model(model) {
        args.push("-m".into());
        args.push(model.to_string());
    }
    if let Some(effort) = map_effort(effort) {
        args.push("-c".into());
        args.push(format!("model_reasoning_effort={effort}"));
    }
    if let Some(cid) = conversation_id.filter(|c| !c.is_empty()) {
        args.push("resume".into());
        args.push(cid.to_string());
    }
    let prompt = match conversation_id {
        None if !system_prompt.trim().is_empty() => {
            format!("{}\n\n{}", system_prompt.trim(), prompt)
        }
        _ => prompt.to_string(),
    };
    args.push(prompt);
    args
}
