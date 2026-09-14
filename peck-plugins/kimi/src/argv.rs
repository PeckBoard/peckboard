pub fn build_cli_args(
    model: Option<&str>,
    prompt: &str,
    conversation_id: Option<&str>,
    system_prompt: &str,
) -> Vec<String> {
    let effective_prompt = if conversation_id.is_none() && !system_prompt.is_empty() {
        format!("{system_prompt}\n\n{prompt}")
    } else {
        prompt.to_string()
    };
    let mut args = vec![
        "--prompt".to_string(),
        effective_prompt,
        "--output-format".to_string(),
        "stream-json".to_string(),
    ];
    if let Some(model) = model.filter(|m| !m.is_empty() && *m != "default" && *m != "auto") {
        let model = model.strip_prefix("kimi:").unwrap_or(model);
        let model = model.split('@').next().unwrap_or(model);
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if let Some(cid) = conversation_id.filter(|c| !c.is_empty()) {
        args.push("--session".to_string());
        args.push(cid.to_string());
    }
    args
}
