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
    // Strip `kimi:` / `@account` BEFORE the default/auto filter: the seed
    // model is stored as `kimi:default`, and comparing the prefixed id
    // against "default" would send `--model default` (a CLI error).
    if let Some(model) = model {
        let model = model.strip_prefix("kimi:").unwrap_or(model);
        let model = model.split('@').next().unwrap_or(model);
        if !model.is_empty() && model != "default" && model != "auto" {
            args.push("--model".to_string());
            args.push(model.to_string());
        }
    }
    if let Some(cid) = conversation_id.filter(|c| !c.is_empty()) {
        args.push("--session".to_string());
        args.push(cid.to_string());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seed model id is stored prefixed (`kimi:default`); it must strip
    /// to the `default` pseudo-model and omit `--model` entirely.
    #[test]
    fn prefixed_default_omits_model_flag() {
        for id in [
            "kimi:default",
            "default",
            "kimi:auto",
            "kimi:",
            "kimi:default@kacc_1",
        ] {
            let args = build_cli_args(Some(id), "hi", None, "");
            assert!(
                !args.iter().any(|a| a == "--model"),
                "{id} must not produce --model: {args:?}"
            );
        }
    }

    #[test]
    fn prefix_and_account_are_stripped_from_real_aliases() {
        let args = build_cli_args(Some("kimi:kimi-for-coding@kacc_1"), "hi", None, "");
        let m = args.iter().position(|a| a == "--model").unwrap();
        assert_eq!(args[m + 1], "kimi-for-coding");
    }

    #[test]
    fn first_turn_prepends_system_prompt_but_resume_does_not() {
        let first = build_cli_args(None, "do it", None, "# Working style");
        assert!(first[1].starts_with("# Working style"));
        assert!(first[1].ends_with("do it"));

        let resume = build_cli_args(None, "do it", Some("sess-7"), "# Working style");
        assert_eq!(resume[1], "do it");
        let s = resume.iter().position(|a| a == "--session").unwrap();
        assert_eq!(resume[s + 1], "sess-7");
    }
}
