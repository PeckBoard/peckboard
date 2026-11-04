pub fn build_cli_args(
    model: &str,
    prompt: &str,
    conversation_id: Option<&str>,
    system_prompt: &str,
    auto_approve: bool,
    image_paths: &[String],
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
    if auto_approve {
        args.push("--force".to_string());
        args.push("--trust".to_string());
    }
    // Hidden from `--help` and marked internal in the CLI, but it is how
    // `--print` attaches images: it fills the message's `selectedImages`.
    for path in image_paths {
        args.push("--image".to_string());
        args.push(path.clone());
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_image_gets_its_own_flag_before_the_prompt() {
        let images = vec!["/w/a.png".to_string(), "/w/b.png".to_string()];
        let args = build_cli_args("cursor:auto", "look", Some("c1"), "", true, &images);
        let sep = args.iter().position(|a| a == "--").unwrap();
        let first = args.iter().position(|a| a == "--image").unwrap();
        assert!(first < sep);
        assert_eq!(args[first + 1], "/w/a.png");
        assert_eq!(args[first + 2], "--image");
        assert_eq!(args[first + 3], "/w/b.png");
        assert_eq!(args.last().unwrap(), "look");
    }

    #[test]
    fn no_images_means_no_flag() {
        let args = build_cli_args("cursor:auto", "look", None, "", true, &[]);
        assert!(!args.iter().any(|a| a == "--image"));
    }
}
