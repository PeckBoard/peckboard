//! Assemble the CLI's task tools into replace-all `todo` snapshots.
//!
//! Minimal port of core's `src/todo.rs::TaskTracker` (crates cannot depend
//! on core): Claude Code ≥ 2.1 reports its list incrementally via
//! `TaskCreate` / `TaskUpdate` (the assigned id only appears in the tool
//! *result*, at `tool_use_result.task.id`); older CLIs used the replace-all
//! `TodoWrite` tool. Both flow through this tracker and come out as full
//! [`TodoItem`] snapshots, status-mapped exactly as core does
//! (`completed`/`done` → `Done`, unknown → `Pending`).
//!
//! Mutations apply at `ToolEnd` time, not `ToolStart`, because a create's id
//! lives in the result and a failed call must not change state.

use crate::event::{TodoItem, TodoStatus};

/// Parse a `TodoWrite` tool input (`{ "todos": [ { content, status,
/// activeForm } ] }`). `None` when the input carries no `todos` array at all
/// (e.g. the empty `{}` placeholder at a streaming tool-block start); an
/// explicitly empty `todos: []` is a valid replace-all and returns `Some`.
pub fn todos_from_todo_write_input(input: &serde_json::Value) -> Option<Vec<TodoItem>> {
    let arr = input.get("todos")?.as_array()?;
    Some(
        arr.iter()
            .filter_map(|item| {
                let content = item.get("content")?.as_str()?.to_string();
                let status = TodoStatus::from_provider(
                    item.get("status").and_then(|v| v.as_str()).unwrap_or(""),
                );
                let active_form = item
                    .get("activeForm")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                Some(TodoItem {
                    content,
                    status,
                    active_form,
                })
            })
            .collect(),
    )
}

#[derive(Debug)]
enum PendingTaskCall {
    Create {
        content: String,
        active_form: Option<String>,
    },
    Update {
        task_id: String,
        status: Option<String>,
        subject: Option<String>,
        active_form: Option<String>,
    },
}

/// Stateful assembler; one per `provider.send` turn. Unlike core's tracker
/// it cannot seed from the session's persisted todos (no DB access
/// plugin-side), so a process respawn mid-conversation starts empty and
/// relies on `TaskCreate` results carrying explicit ids.
#[derive(Debug, Default)]
pub struct TaskTracker {
    /// Tasks in creation order, keyed by the provider's task id.
    tasks: Vec<(String, TodoItem)>,
    /// Task tool calls seen at `ToolStart`, awaiting their `ToolEnd`.
    pending: std::collections::HashMap<String, PendingTaskCall>,
}

impl TaskTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a tool invocation. `TodoWrite` applies immediately (its input is
    /// the whole list) and returns the new snapshot; `TaskCreate` /
    /// `TaskUpdate` are parked until [`Self::on_tool_end`] confirms them.
    pub fn on_tool_start(
        &mut self,
        tool_use_id: &str,
        name: &str,
        input: &serde_json::Value,
    ) -> Option<Vec<TodoItem>> {
        match name {
            "TodoWrite" => {
                let todos = todos_from_todo_write_input(input)?;
                self.tasks = todos
                    .iter()
                    .enumerate()
                    .map(|(idx, item)| ((idx + 1).to_string(), item.clone()))
                    .collect();
                Some(todos)
            }
            "TaskCreate" => {
                let content = input.get("subject")?.as_str()?.to_string();
                let active_form = input
                    .get("activeForm")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                self.pending.insert(
                    tool_use_id.to_string(),
                    PendingTaskCall::Create {
                        content,
                        active_form,
                    },
                );
                None
            }
            "TaskUpdate" => {
                let task_id = json_str(input, "taskId")?;
                self.pending.insert(
                    tool_use_id.to_string(),
                    PendingTaskCall::Update {
                        task_id,
                        status: json_str(input, "status"),
                        subject: json_str(input, "subject"),
                        active_form: json_str(input, "activeForm"),
                    },
                );
                None
            }
            _ => None,
        }
    }

    /// Feed a tool completion. Applies the pending call for `tool_use_id`
    /// (if any) and returns the new full snapshot when state changed.
    /// `result` is the CLI's structured `tool_use_result` for this call —
    /// for `TaskCreate` it carries the assigned id at `task.id`.
    pub fn on_tool_end(
        &mut self,
        tool_use_id: &str,
        errored: bool,
        result: Option<&serde_json::Value>,
    ) -> Option<Vec<TodoItem>> {
        let call = self.pending.remove(tool_use_id)?;
        if errored {
            return None;
        }
        match call {
            PendingTaskCall::Create {
                content,
                active_form,
            } => {
                let id = result
                    .and_then(|r| r.get("task"))
                    .and_then(|t| t.get("id"))
                    .and_then(task_id_string)
                    .unwrap_or_else(|| self.next_id());
                let item = TodoItem {
                    content,
                    status: TodoStatus::Pending,
                    active_form,
                };
                match self.tasks.iter_mut().find(|(tid, _)| *tid == id) {
                    Some((_, existing)) => *existing = item,
                    None => self.tasks.push((id, item)),
                }
                Some(self.snapshot())
            }
            PendingTaskCall::Update {
                task_id,
                status,
                subject,
                active_form,
            } => {
                let idx = self.tasks.iter().position(|(tid, _)| *tid == task_id)?;
                if status.as_deref() == Some("deleted") {
                    self.tasks.remove(idx);
                    return Some(self.snapshot());
                }
                let mut changed = false;
                let item = &mut self.tasks[idx].1;
                if let Some(status) = status {
                    item.status = TodoStatus::from_provider(&status);
                    changed = true;
                }
                if let Some(subject) = subject {
                    item.content = subject;
                    changed = true;
                }
                if let Some(active_form) = active_form {
                    item.active_form = Some(active_form);
                    changed = true;
                }
                changed.then(|| self.snapshot())
            }
        }
    }

    fn snapshot(&self) -> Vec<TodoItem> {
        self.tasks.iter().map(|(_, item)| item.clone()).collect()
    }

    /// Fallback id when a create's result is missing (e.g. a synthesized
    /// `ToolEnd`): mirror the CLI's sequential counter.
    fn next_id(&self) -> String {
        let max = self
            .tasks
            .iter()
            .filter_map(|(tid, _)| tid.parse::<u64>().ok())
            .max()
            .unwrap_or(0);
        (max + 1).to_string()
    }
}

fn json_str(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

/// Task ids arrive as JSON strings today (`{"task":{"id":"1"}}`) but accept
/// a bare number too so a serialization change upstream doesn't drop
/// captures.
fn task_id_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn todo_write_maps_statuses_like_core() {
        let todos = todos_from_todo_write_input(&json!({
            "todos": [
                { "content": "a", "status": "pending", "activeForm": "doing a" },
                { "content": "b", "status": "in_progress" },
                { "content": "c", "status": "completed" },
                { "content": "d", "status": "done" },
                { "content": "e", "status": "???" },
            ]
        }))
        .unwrap();
        let statuses: Vec<TodoStatus> = todos.iter().map(|t| t.status).collect();
        assert_eq!(
            statuses,
            vec![
                TodoStatus::Pending,
                TodoStatus::InProgress,
                TodoStatus::Done,
                TodoStatus::Done,
                TodoStatus::Pending,
            ]
        );
        assert_eq!(todos[0].active_form.as_deref(), Some("doing a"));
        // No `todos` array at all (streaming placeholder) is not a snapshot.
        assert!(todos_from_todo_write_input(&json!({})).is_none());
        // An explicitly empty list is a valid replace-all.
        assert_eq!(
            todos_from_todo_write_input(&json!({ "todos": [] })),
            Some(vec![])
        );
    }

    #[test]
    fn task_tracker_assembles_create_and_update_deltas() {
        let mut t = TaskTracker::new();
        assert!(
            t.on_tool_start("t1", "TaskCreate", &json!({ "subject": "write tests" }))
                .is_none()
        );
        let snap = t
            .on_tool_end("t1", false, Some(&json!({ "task": { "id": "1" } })))
            .unwrap();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].content, "write tests");
        assert_eq!(snap[0].status, TodoStatus::Pending);

        t.on_tool_start(
            "t2",
            "TaskUpdate",
            &json!({ "taskId": "1", "status": "completed" }),
        );
        let snap = t.on_tool_end("t2", false, None).unwrap();
        assert_eq!(snap[0].status, TodoStatus::Done);

        // Errored calls must not change state.
        t.on_tool_start(
            "t3",
            "TaskUpdate",
            &json!({ "taskId": "1", "status": "deleted" }),
        );
        assert!(t.on_tool_end("t3", true, None).is_none());
        assert_eq!(t.snapshot().len(), 1);
    }
}
