use serde::{Deserialize, Serialize};

use crate::db::models::Event;

/// What the scheduler determines the worker should do next, based on events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkerIntent {
    /// The agent requested completing the current step.
    CompleteStep { handoff_context: Option<String> },
    /// The agent requested finishing the card entirely.
    Finish { summary: Option<String> },
    /// The agent requested marking the card as won't-do.
    WontDo { reason: String },
    /// The agent asked the user a question; block until answered.
    AskUser { question: String },
    /// No special request detected; continue normal operation.
    Continue,
}

/// Walk the event tail (bounded by the latest `step-change` event) and look
/// for `*-requested` events that indicate what the worker intends to do.
/// Returns the most recent intent, or `None` if no relevant events are found.
pub fn derive_worker_intent(events: &[Event]) -> Option<WorkerIntent> {
    // Find the boundary: the latest step-change event index.
    let boundary = events
        .iter()
        .rposition(|e| e.kind == "step-change")
        .map(|i| i + 1)
        .unwrap_or(0);

    let window = &events[boundary..];

    // Walk backwards to find the most recent *-requested event.
    for event in window.iter().rev() {
        match event.kind.as_str() {
            "complete-step-requested" => {
                let data: serde_json::Value = serde_json::from_str(&event.data).unwrap_or_default();
                let handoff_context = data
                    .get("handoffContext")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                return Some(WorkerIntent::CompleteStep { handoff_context });
            }
            "finish-requested" => {
                let data: serde_json::Value = serde_json::from_str(&event.data).unwrap_or_default();
                let summary = data
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
                return Some(WorkerIntent::Finish { summary });
            }
            "wont-do-requested" => {
                let data: serde_json::Value = serde_json::from_str(&event.data).unwrap_or_default();
                let reason = data
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("no reason given")
                    .to_string();
                return Some(WorkerIntent::WontDo { reason });
            }
            "ask-user-requested" => {
                let data: serde_json::Value = serde_json::from_str(&event.data).unwrap_or_default();
                let question = data
                    .get("question")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                return Some(WorkerIntent::AskUser { question });
            }
            _ => {}
        }
    }

    None
}

/// Find the conversation ID from the most recent `agent-start` event, which
/// can be used to resume a conversation.
pub fn find_resume_conversation_id(events: &[Event]) -> Option<String> {
    for event in events.iter().rev() {
        if event.kind == "agent-start" {
            let data: serde_json::Value = serde_json::from_str(&event.data).unwrap_or_default();
            if let Some(cid) = data.get("conversationId").and_then(|v| v.as_str()) {
                if !cid.is_empty() {
                    return Some(cid.to_string());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_derive_intent_complete_step() {
        let events = vec![
            make_event("agent-start", r#"{"model":"opus"}"#),
            make_event(
                "complete-step-requested",
                r#"{"cardId":"c1","handoffContext":"done with step"}"#,
            ),
        ];
        let intent = derive_worker_intent(&events);
        assert_eq!(
            intent,
            Some(WorkerIntent::CompleteStep {
                handoff_context: Some("done with step".into()),
            })
        );
    }

    #[test]
    fn test_derive_intent_finish() {
        let events = vec![
            make_event("agent-start", r#"{"model":"opus"}"#),
            make_event(
                "finish-requested",
                r#"{"cardId":"c1","summary":"all done"}"#,
            ),
        ];
        let intent = derive_worker_intent(&events);
        assert_eq!(
            intent,
            Some(WorkerIntent::Finish {
                summary: Some("all done".into()),
            })
        );
    }

    #[test]
    fn test_derive_intent_wont_do() {
        let events = vec![make_event(
            "wont-do-requested",
            r#"{"cardId":"c1","reason":"blocked by external dep"}"#,
        )];
        let intent = derive_worker_intent(&events);
        assert_eq!(
            intent,
            Some(WorkerIntent::WontDo {
                reason: "blocked by external dep".into(),
            })
        );
    }

    #[test]
    fn test_derive_intent_ask_user() {
        let events = vec![make_event(
            "ask-user-requested",
            r#"{"question":"Which database?"}"#,
        )];
        let intent = derive_worker_intent(&events);
        assert_eq!(
            intent,
            Some(WorkerIntent::AskUser {
                question: "Which database?".into(),
            })
        );
    }

    #[test]
    fn test_derive_intent_none_with_no_requests() {
        let events = vec![
            make_event("agent-start", r#"{"model":"opus"}"#),
            make_event("agent-text", r#"{"text":"working..."}"#),
            make_event("agent-end", r#"{"status":"complete"}"#),
        ];
        let intent = derive_worker_intent(&events);
        assert_eq!(intent, None);
    }

    #[test]
    fn test_derive_intent_empty() {
        assert_eq!(derive_worker_intent(&[]), None);
    }

    #[test]
    fn test_derive_intent_bounded_by_step_change() {
        let events = vec![
            make_event(
                "complete-step-requested",
                r#"{"cardId":"c1","handoffContext":"old"}"#,
            ),
            make_event("step-change", r#"{"from":"todo","to":"in-progress"}"#),
            make_event("agent-start", r#"{"model":"opus"}"#),
        ];
        // The complete-step-requested is before the step-change boundary, so
        // it should not be visible.
        let intent = derive_worker_intent(&events);
        assert_eq!(intent, None);
    }

    #[test]
    fn test_derive_intent_takes_most_recent() {
        let events = vec![
            make_event("ask-user-requested", r#"{"question":"old question"}"#),
            make_event(
                "complete-step-requested",
                r#"{"cardId":"c1","handoffContext":"latest"}"#,
            ),
        ];
        let intent = derive_worker_intent(&events);
        assert_eq!(
            intent,
            Some(WorkerIntent::CompleteStep {
                handoff_context: Some("latest".into()),
            })
        );
    }

    #[test]
    fn test_find_resume_conversation_id_found() {
        let events = vec![
            make_event(
                "agent-start",
                r#"{"model":"opus","conversationId":"conv-123"}"#,
            ),
            make_event("agent-text", r#"{"text":"hello"}"#),
            make_event("agent-end", r#"{"status":"complete"}"#),
            make_event(
                "agent-start",
                r#"{"model":"opus","conversationId":"conv-456"}"#,
            ),
        ];
        assert_eq!(
            find_resume_conversation_id(&events),
            Some("conv-456".into())
        );
    }

    #[test]
    fn test_find_resume_conversation_id_none() {
        let events = vec![
            make_event("agent-start", r#"{"model":"opus"}"#),
            make_event("agent-text", r#"{"text":"hello"}"#),
        ];
        assert_eq!(find_resume_conversation_id(&events), None);
    }

    #[test]
    fn test_find_resume_conversation_id_empty() {
        assert_eq!(find_resume_conversation_id(&[]), None);
    }

    #[test]
    fn test_find_resume_conversation_id_skips_empty_string() {
        let events = vec![make_event(
            "agent-start",
            r#"{"model":"opus","conversationId":""}"#,
        )];
        assert_eq!(find_resume_conversation_id(&events), None);
    }
}
