//! In-flight background subagent tracking for one CLI child.
//!
//! The CLI's native Task/Agent tool can launch agents in the background:
//! the launching turn settles with a `result` frame while the agent keeps
//! running inside the CLI process, and a `task-notification` user turn is
//! injected when it finishes. Killing the child at the settling result
//! (one `provider.send` = one turn) therefore orphaned every in-flight
//! background agent — the next `--resume` replayed only a "stopped, no
//! completion record" notification and the work was lost. `send::run` now
//! lingers after the settling result until this tracker drains (or a stop
//! request / the linger cap fires).
//!
//! Frame shapes were captured from claude 2.1.226 transcripts:
//! - launch: the `Task`/`Agent` tool result's structured sibling
//!   (`tool_use_result` on the raw line) is
//!   `{"isAsync": true, "status": "async_launched", "agentId": "..."}`.
//! - resume: `SendMessage` to a finished agent restarts it and returns
//!   `{"success": true, "resumedAgentId": "..."}`.
//! - settle: the CLI injects a user frame stamped
//!   `origin: {"kind": "task-notification"}` whose text carries
//!   `<task-id>...</task-id>` (any `<status>` — completed and stopped both
//!   mean the agent is no longer running).
//! - manual stop: a `TaskStop` tool call with input `{"task_id": "..."}`
//!   settles that id with no notification to follow.

use std::collections::{BTreeSet, HashMap};

use serde_json::Value;

#[derive(Default)]
pub struct BackgroundTracker {
    /// Ids of background agents launched or resumed by this CLI child and
    /// not yet settled. Ordered so wind-down notes read deterministically.
    pending: BTreeSet<String>,
    /// tool_use_id → task id for TaskStop calls awaiting their result.
    stop_intents: HashMap<String, String>,
}

impl BackgroundTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    pub fn pending_ids(&self) -> Vec<String> {
        self.pending.iter().cloned().collect()
    }

    /// Record a TaskStop intent so its success (seen at tool_end) settles
    /// the target id.
    pub fn on_tool_start(&mut self, tool_use_id: &str, name: &str, input: &Value) {
        if name == "TaskStop"
            && let Some(id) = input.get("task_id").and_then(|v| v.as_str())
        {
            self.stop_intents
                .insert(tool_use_id.to_string(), id.to_string());
        }
    }

    /// Inspect a tool result's structured sibling (`tool_use_result` on the
    /// raw stream line) for a background launch/resume, and settle TaskStop
    /// intents.
    pub fn on_tool_end(
        &mut self,
        tool_use_id: &str,
        is_error: bool,
        tool_use_result: Option<&Value>,
    ) {
        if let Some(id) = self.stop_intents.remove(tool_use_id)
            && !is_error
        {
            self.pending.remove(&id);
        }
        if is_error {
            return;
        }
        let Some(res) = tool_use_result.filter(|v| v.is_object()) else {
            return;
        };
        let is_async = res
            .get("isAsync")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if is_async {
            let id = ["agentId", "taskId", "task_id", "runId"]
                .iter()
                .find_map(|k| res.get(*k).and_then(|v| v.as_str()));
            if let Some(id) = id {
                self.pending.insert(id.to_string());
            }
        }
        let resumed_ok = res.get("success").and_then(|v| v.as_bool()) != Some(false);
        if resumed_ok && let Some(id) = res.get("resumedAgentId").and_then(|v| v.as_str()) {
            self.pending.insert(id.to_string());
        }
    }

    /// Settle from a `task-notification` user frame. Returns the settled id
    /// when the frame was a notification for a tracked agent.
    pub fn on_stream_line(&mut self, json: &Value) -> Option<String> {
        if json.get("type").and_then(|v| v.as_str()) != Some("user") {
            return None;
        }
        if json
            .get("origin")
            .and_then(|o| o.get("kind"))
            .and_then(|k| k.as_str())
            != Some("task-notification")
        {
            return None;
        }
        let content = json.get("message").and_then(|m| m.get("content"))?;
        let text = match content {
            Value::String(s) => s.clone(),
            Value::Array(blocks) => blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => return None,
        };
        let id = text
            .split_once("<task-id>")
            .and_then(|(_, rest)| rest.split_once("</task-id>"))
            .map(|(id, _)| id.trim().to_string())?;
        if self.pending.remove(&id) {
            Some(id)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Launch shape captured live from claude 2.1.226.
    fn launch_result(agent_id: &str) -> Value {
        json!({
            "isAsync": true,
            "status": "async_launched",
            "agentId": agent_id,
            "description": "Map plugin architecture",
            "resolvedModel": "claude-opus-5",
        })
    }

    /// Notification user frame captured live from claude 2.1.226.
    fn notification_frame(task_id: &str) -> Value {
        json!({
            "type": "user",
            "origin": { "kind": "task-notification" },
            "message": {
                "role": "user",
                "content": format!(
                    "<task-notification>\n<task-id>{task_id}</task-id>\n\
                     <status>completed</status>\n<summary>done</summary>\n\
                     </task-notification>"
                ),
            },
        })
    }

    #[test]
    fn launch_then_notification_settles() {
        let mut t = BackgroundTracker::new();
        t.on_tool_end("tu1", false, Some(&launch_result("a45ed88e4cb59c48a")));
        assert_eq!(t.pending(), 1);
        assert_eq!(
            t.on_stream_line(&notification_frame("a45ed88e4cb59c48a")),
            Some("a45ed88e4cb59c48a".to_string())
        );
        assert_eq!(t.pending(), 0);
    }

    #[test]
    fn errored_launch_and_plain_tools_track_nothing() {
        let mut t = BackgroundTracker::new();
        t.on_tool_end("tu1", true, Some(&launch_result("a1")));
        t.on_tool_end("tu2", false, Some(&json!({"ok": true})));
        t.on_tool_end("tu3", false, None);
        assert_eq!(t.pending(), 0);
    }

    #[test]
    fn send_message_resume_reopens_the_agent() {
        let mut t = BackgroundTracker::new();
        // Captured shape: resuming a finished agent via SendMessage.
        t.on_tool_end(
            "tu1",
            false,
            Some(&json!({
                "success": true,
                "message": "resumed from transcript in the background",
                "resumedAgentId": "a45ed88e4cb59c48a",
            })),
        );
        assert_eq!(t.pending_ids(), vec!["a45ed88e4cb59c48a".to_string()]);
    }

    #[test]
    fn task_stop_settles_without_a_notification() {
        let mut t = BackgroundTracker::new();
        t.on_tool_end("tu1", false, Some(&launch_result("b5snf8l6o")));
        t.on_tool_start("tu2", "TaskStop", &json!({"task_id": "b5snf8l6o"}));
        t.on_tool_end("tu2", false, None);
        assert_eq!(t.pending(), 0);
    }

    #[test]
    fn failed_task_stop_keeps_the_agent_pending() {
        let mut t = BackgroundTracker::new();
        t.on_tool_end("tu1", false, Some(&launch_result("b5snf8l6o")));
        t.on_tool_start("tu2", "TaskStop", &json!({"task_id": "b5snf8l6o"}));
        t.on_tool_end("tu2", true, None);
        assert_eq!(t.pending(), 1);
    }

    #[test]
    fn foreign_and_unknown_notifications_are_ignored() {
        let mut t = BackgroundTracker::new();
        t.on_tool_end("tu1", false, Some(&launch_result("a1")));
        // Unknown id: not ours.
        assert_eq!(t.on_stream_line(&notification_frame("zz")), None);
        // Ordinary user frame without the origin stamp.
        assert_eq!(
            t.on_stream_line(&json!({
                "type": "user",
                "message": {"role": "user", "content": "<task-id>a1</task-id>"},
            })),
            None
        );
        assert_eq!(t.pending(), 1);
    }
}
