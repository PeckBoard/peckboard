pub fn build_cli_args(
    model: &str,
    prompt: &str,
    conversation_id: Option<&str>,
    system_prompt: &str,
) -> Vec<String> {
    let mut args = vec![
        "--print".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--stream-partial-output".to_string(),
    ];
    let model = model.strip_prefix("cursor:").unwrap_or(model);
    let model = model.split('@').next().unwrap_or(model);
    if !model.is_empty() && model != "auto" {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if let Some(cid) = conversation_id.filter(|c| !c.is_empty()) {
        args.push("--resume".to_string());
        args.push(cid.to_string());
    }
    args.push("--force".to_string());
    args.push("--trust".to_string());
    args.push("--".to_string());
    let prompt = match conversation_id {
        None if !system_prompt.trim().is_empty() => {
            format!("{}\n\n{}", system_prompt.trim(), prompt)
        }
        _ => prompt.to_string(),
    };
    args.push(prompt);
    args
}
