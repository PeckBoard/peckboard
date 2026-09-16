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
    image_paths: &[String],
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
    for path in image_paths {
        args.push("--image".into());
        args.push(path.clone());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_args_includes_one_image_flag_per_staged_file() {
        let args = build_cli_args(
            "gpt-5.6",
            "see",
            None,
            None,
            "",
            &["/tmp/a.png".into(), "/tmp/b.jpg".into()],
            &[],
        );
        let images: Vec<_> = args
            .windows(2)
            .filter(|w| w[0] == "--image")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(images, vec!["/tmp/a.png", "/tmp/b.jpg"]);
        assert_eq!(args.last().unwrap(), "see");
    }

    #[test]
    fn images_ride_after_resume_so_they_apply_to_the_new_turn() {
        let args = build_cli_args(
            "auto",
            "hi",
            Some("thread-9"),
            None,
            "",
            &["/tmp/a.png".into()],
            &[],
        );
        let resume_at = args.iter().position(|a| a == "resume").unwrap();
        let image_at = args.iter().position(|a| a == "--image").unwrap();
        assert!(resume_at < image_at);
    }
}
