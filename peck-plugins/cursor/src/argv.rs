/// Cursor's default base-prompt text. `cursor-agent` has no flag to disable a
/// built-in tool, so its question tool can't be removed — this steers the model
/// to Peckboard's `ask_user`, which is how a question reaches the user here.
pub(crate) const ASK_USER_PROMPT: &str = r#"
# Asking the User Questions

You run inside Peckboard, a remote web UI with no terminal. To ask the user anything, call the `ask_user` tool from the `peckboard` MCP server. NEVER use Cursor's built-in AskQuestion tool: the user answers in Peckboard, and that tool does not reach them there. Don't ask in plain text either while `ask_user` is available.

Input: `{"questions":[{question, header, multiSelect?, options?}]}`
- `question` (string, required): the question text.
- `header` (string, required): short category label ("Setup", "Confirm", "Input").
- `multiSelect` (bool, default false): false = pick one, true = pick several.
- `options` (array of `{label, description}`): renders multiple choice; `description` is required ("" if nothing to add). OMIT `options` for a free-form text input.
- Exactly ONE question per call. For several questions, make sequential calls and wait for each answer.

Prefer multiple choice when the valid answers are known, and include an "Other" option. Wait for the answer; never assume it. If `ask_user` is not available, state the open question in your final message and stop.
"#;

/// Resumed turns don't get the system prompt again, so this is what reaches
/// chats that started before [`ASK_USER_PROMPT`] existed.
const ASK_USER_REMINDER: &str = "[Peckboard: to ask the user anything, call the `ask_user` tool from the `peckboard` MCP server. Never use Cursor's built-in AskQuestion tool; it does not reach the user here.]";

pub fn build_cli_args(
    model: &str,
    prompt: &str,
    conversation_id: Option<&str>,
    system_prompt: &str,
    base_prompt_overridden: bool,
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
        // Also covers `~/.cursor/mcp.json`, which the send path can't list
        // for `mcp enable`.
        args.push("--approve-mcps".to_string());
    }
    // Hidden from `--help` and marked internal in the CLI, but it is how
    // `--print` attaches images: it fills the message's `selectedImages`.
    for path in image_paths {
        args.push("--image".to_string());
        args.push(path.clone());
    }
    args.push("--".to_string());
    let prompt = match conversation_id {
        None => {
            // A user override of the base prompt already arrived in
            // `system_prompt` and replaces these rules outright.
            let rules = if base_prompt_overridden {
                ""
            } else {
                ASK_USER_PROMPT
            };
            let standing = format!("{rules}{system_prompt}");
            match standing.trim() {
                "" => prompt.to_string(),
                standing => format!("{standing}\n\n{prompt}"),
            }
        }
        Some(_) if base_prompt_overridden => prompt.to_string(),
        Some(_) => format!("{ASK_USER_REMINDER}\n\n{prompt}"),
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
        let args = build_cli_args("cursor:auto", "look", Some("c1"), "", true, true, &images);
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
        let args = build_cli_args("cursor:auto", "look", None, "", false, true, &[]);
        assert!(!args.iter().any(|a| a == "--image"));
    }

    fn sent_prompt(conversation_id: Option<&str>, overridden: bool) -> String {
        build_cli_args(
            "cursor:auto",
            "hello",
            conversation_id,
            "BASE",
            overridden,
            true,
            &[],
        )
        .pop()
        .unwrap()
    }

    #[test]
    fn new_chats_get_the_ask_user_rules_ahead_of_the_base_prompt() {
        let prompt = sent_prompt(None, false);
        assert!(prompt.contains("call the `ask_user` tool"), "{prompt}");
        assert!(prompt.contains("NEVER use Cursor's built-in AskQuestion"));
        assert!(prompt.ends_with("BASE\n\nhello"), "{prompt}");
    }

    #[test]
    fn resumed_chats_get_the_reminder_without_the_system_prompt() {
        let prompt = sent_prompt(Some("c1"), false);
        assert_eq!(prompt, format!("{ASK_USER_REMINDER}\n\nhello"));
    }

    #[test]
    fn a_base_prompt_override_replaces_the_rules_and_the_reminder() {
        assert_eq!(sent_prompt(None, true), "BASE\n\nhello");
        assert_eq!(sent_prompt(Some("c1"), true), "hello");
    }
}
