use crate::db::models::{Card, Event, Project};

/// Build the system prompt for a worker agent given its assignment context.
pub fn build_worker_prompt(
    project: &Project,
    card: &Card,
    step: &str,
    handoff_context: Option<&str>,
) -> String {
    let mut prompt = String::new();

    prompt.push_str(&format!(
        "You are a worker agent on the project \"{}\".\n\n",
        project.name
    ));

    prompt.push_str("## Project Context\n\n");
    prompt.push_str(&project.context);
    prompt.push_str("\n\n");

    prompt.push_str("## Your Assignment\n\n");
    prompt.push_str(&format!("**Card:** {}\n", card.title));
    prompt.push_str(&format!("**Current Step:** {}\n", step));
    prompt.push_str(&format!("**Description:**\n{}\n\n", card.description));

    if let Some(workflow) = &card.workflow {
        prompt.push_str(&format!("**Workflow:** {}\n\n", workflow));
    }

    if let Some(ctx) = handoff_context {
        prompt.push_str("## Handoff Context from Previous Step\n\n");
        prompt.push_str(ctx);
        prompt.push_str("\n\n");
    }

    prompt.push_str("## Available Tools\n\n");
    prompt.push_str(
        "- `complete_step` — Mark the current step as done and advance to the next step. \
         Include a handoff_context summarizing what you did.\n",
    );
    prompt.push_str(
        "- `finish_card` — Mark the entire card as finished (use when all steps are done).\n",
    );
    prompt.push_str(
        "- `wont_do_card` — Mark the card as won't-do if it cannot or should not be completed.\n",
    );
    prompt.push_str(
        "- `ask_user` — Ask the user a question if you need clarification. This will block \
         until the user responds.\n",
    );
    prompt.push_str("- `create_card` — Create a new card in this project.\n");
    prompt.push_str("- `list_cards` — List all cards in this project.\n");
    prompt.push_str("- `write_report` — Write a report or note for human review.\n");
    prompt.push_str("\n");

    prompt.push_str("## Instructions\n\n");
    prompt.push_str(
        "Work on the current step. When you are done, call `complete_step` with a handoff \
         context describing what you accomplished. If this is the final step, call `finish_card` \
         instead. If you cannot complete the task, call `wont_do_card` with a reason. If you \
         need information from the user, call `ask_user`.\n",
    );

    prompt
}

/// Given the current step and an ordered list of workflow steps, find the next
/// step. Returns `None` if `current_step` is the last step or not found.
pub fn find_next_step(current_step: &str, workflow_steps: &[String]) -> Option<String> {
    let pos = workflow_steps.iter().position(|s| s == current_step)?;
    workflow_steps.get(pos + 1).cloned()
}

/// Walk the event tail and count consecutive crashes. Returns
/// `(crash_count, should_block)`. Blocks on 4th consecutive crash.
/// The counter resets on an `agent-end` with status "complete" or on a
/// `step-change` event.
pub fn detect_retry_loop(events: &[Event]) -> (u32, bool) {
    let mut crash_count: u32 = 0;

    // Walk events in order (oldest to newest).
    for event in events {
        match event.kind.as_str() {
            "agent-end" => {
                // Parse data to check status.
                if let Ok(data) = serde_json::from_str::<serde_json::Value>(&event.data) {
                    match data.get("status").and_then(|s| s.as_str()) {
                        Some("crashed") => {
                            crash_count += 1;
                        }
                        Some("complete") => {
                            crash_count = 0;
                        }
                        _ => {}
                    }
                }
            }
            "step-change" => {
                crash_count = 0;
            }
            _ => {}
        }
    }

    let should_block = crash_count >= 4;
    (crash_count, should_block)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{Card, Project};

    fn sample_project() -> Project {
        Project {
            id: "p1".into(),
            name: "Test Project".into(),
            context: "Build a web app with Rust.".into(),
            folder_id: "f1".into(),
            worker_count: 2,
            status: "active".into(),
            default_workflow: Some("default".into()),
            model: None,
            effort: None,
            parallel_instructions: false,
            created_at: "2025-01-01T00:00:00Z".into(),
            last_accessed_at: "2025-01-01T00:00:00Z".into(),
        }
    }

    fn sample_card() -> Card {
        Card {
            id: "c1".into(),
            project_id: "p1".into(),
            title: "Implement auth".into(),
            description: "Add JWT-based authentication.".into(),
            step: "in-progress".into(),
            priority: 1,
            workflow: Some("default".into()),
            model: None,
            effort: None,
            worker_session_id: None,
            last_worker_session_id: None,
            handoff_context: None,
            blocked: false,
            block_reason: None,
            created_at: "2025-01-01T00:00:00Z".into(),
            updated_at: "2025-01-01T00:00:00Z".into(),
        }
    }

    fn make_event(kind: &str, data: &str) -> Event {
        Event {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: "s1".into(),
            seq: 0,
            ts: 0,
            kind: kind.into(),
            data: data.into(),
        }
    }

    #[test]
    fn test_build_worker_prompt_basic() {
        let prompt = build_worker_prompt(&sample_project(), &sample_card(), "in-progress", None);
        assert!(prompt.contains("Test Project"));
        assert!(prompt.contains("Implement auth"));
        assert!(prompt.contains("in-progress"));
        assert!(prompt.contains("Build a web app with Rust."));
    }

    #[test]
    fn test_build_worker_prompt_with_handoff() {
        let prompt = build_worker_prompt(
            &sample_project(),
            &sample_card(),
            "review",
            Some("Auth module is at src/auth/"),
        );
        assert!(prompt.contains("Handoff Context"));
        assert!(prompt.contains("Auth module is at src/auth/"));
    }

    #[test]
    fn test_find_next_step() {
        let steps: Vec<String> = vec![
            "todo".into(),
            "in-progress".into(),
            "review".into(),
            "done".into(),
        ];

        assert_eq!(find_next_step("todo", &steps), Some("in-progress".into()));
        assert_eq!(find_next_step("in-progress", &steps), Some("review".into()));
        assert_eq!(find_next_step("review", &steps), Some("done".into()));
        assert_eq!(find_next_step("done", &steps), None);
        assert_eq!(find_next_step("nonexistent", &steps), None);
    }

    #[test]
    fn test_find_next_step_empty() {
        let steps: Vec<String> = vec![];
        assert_eq!(find_next_step("todo", &steps), None);
    }

    #[test]
    fn test_detect_retry_loop_no_crashes() {
        let events = vec![make_event("agent-end", r#"{"status":"complete"}"#)];
        let (count, blocked) = detect_retry_loop(&events);
        assert_eq!(count, 0);
        assert!(!blocked);
    }

    #[test]
    fn test_detect_retry_loop_under_threshold() {
        let events = vec![
            make_event("agent-end", r#"{"status":"crashed","reason":"timeout"}"#),
            make_event("agent-end", r#"{"status":"crashed","reason":"timeout"}"#),
            make_event("agent-end", r#"{"status":"crashed","reason":"timeout"}"#),
        ];
        let (count, blocked) = detect_retry_loop(&events);
        assert_eq!(count, 3);
        assert!(!blocked);
    }

    #[test]
    fn test_detect_retry_loop_at_threshold() {
        let events = vec![
            make_event("agent-end", r#"{"status":"crashed","reason":"oom"}"#),
            make_event("agent-end", r#"{"status":"crashed","reason":"oom"}"#),
            make_event("agent-end", r#"{"status":"crashed","reason":"oom"}"#),
            make_event("agent-end", r#"{"status":"crashed","reason":"oom"}"#),
        ];
        let (count, blocked) = detect_retry_loop(&events);
        assert_eq!(count, 4);
        assert!(blocked);
    }

    #[test]
    fn test_detect_retry_loop_reset_on_complete() {
        let events = vec![
            make_event("agent-end", r#"{"status":"crashed","reason":"x"}"#),
            make_event("agent-end", r#"{"status":"crashed","reason":"x"}"#),
            make_event("agent-end", r#"{"status":"crashed","reason":"x"}"#),
            make_event("agent-end", r#"{"status":"complete"}"#),
            make_event("agent-end", r#"{"status":"crashed","reason":"x"}"#),
        ];
        let (count, blocked) = detect_retry_loop(&events);
        assert_eq!(count, 1);
        assert!(!blocked);
    }

    #[test]
    fn test_detect_retry_loop_reset_on_step_change() {
        let events = vec![
            make_event("agent-end", r#"{"status":"crashed","reason":"x"}"#),
            make_event("agent-end", r#"{"status":"crashed","reason":"x"}"#),
            make_event("agent-end", r#"{"status":"crashed","reason":"x"}"#),
            make_event("step-change", r#"{"from":"todo","to":"in-progress"}"#),
            make_event("agent-end", r#"{"status":"crashed","reason":"x"}"#),
            make_event("agent-end", r#"{"status":"crashed","reason":"x"}"#),
        ];
        let (count, blocked) = detect_retry_loop(&events);
        assert_eq!(count, 2);
        assert!(!blocked);
    }

    #[test]
    fn test_detect_retry_loop_empty() {
        let (count, blocked) = detect_retry_loop(&[]);
        assert_eq!(count, 0);
        assert!(!blocked);
    }
}
