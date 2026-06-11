//! Canonical representation of a trackable work item — a "todo"/"task".
//!
//! Peckboard treats todos and tasks as one unified concept: a single work
//! item with a three-state lifecycle, `Pending → In Progress → Done`. There is
//! no separate "task" type; everywhere in the codebase a trackable unit of
//! work is a [`TodoItem`] in this lifecycle.
//!
//! The shape here is deliberately provider-agnostic. Claude Code ≥ 2.1
//! surfaces its list incrementally via the `TaskCreate` / `TaskUpdate` tools
//! (assembled by [`TaskTracker`]); older CLIs used the replace-all `TodoWrite`
//! tool. A third-party Extism provider could report work items differently
//! still. All of them normalize onto [`TodoStatus`] so downstream consumers
//! (routes, frontend) never need to know which agent produced the snapshot.

use serde::{Deserialize, Serialize};

/// The canonical lifecycle state of a work item.
///
/// Provider-native status tokens map onto this enum (see
/// [`TodoStatus::from_provider`]). For Claude's `TodoWrite`:
/// `pending → Pending`, `in_progress → InProgress`, `completed → Done`.
///
/// Wire form is snake_case (`pending` / `in_progress` / `done`) so the JSON is
/// a stable machine token; [`TodoStatus::label`] gives the human-facing
/// "Pending" / "In Progress" / "Done" for rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
}

impl TodoStatus {
    /// Normalize a provider-native status token onto the canonical lifecycle.
    ///
    /// Recognizes Claude's `pending` / `in_progress` / `completed` (and treats
    /// `done` as a synonym for completed). Anything unrecognized falls back to
    /// `Pending` rather than dropping the item — an unknown status should never
    /// make a work item disappear.
    pub fn from_provider(raw: &str) -> Self {
        match raw {
            "in_progress" => TodoStatus::InProgress,
            "completed" | "done" => TodoStatus::Done,
            _ => TodoStatus::Pending,
        }
    }

    /// Human-facing label for this state.
    pub fn label(self) -> &'static str {
        match self {
            TodoStatus::Pending => "Pending",
            TodoStatus::InProgress => "In Progress",
            TodoStatus::Done => "Done",
        }
    }
}

/// A single trackable work item in a session's todo snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    /// The work to be done, e.g. "Run the test suite".
    pub content: String,
    /// Where this item sits in the lifecycle.
    pub status: TodoStatus,
    /// Present-tense form a provider shows while the item is in progress
    /// (e.g. "Running the test suite"). Optional — not every provider supplies
    /// it. Serialized as `activeForm` to match the `TodoWrite` field name.
    #[serde(
        default,
        rename = "activeForm",
        skip_serializing_if = "Option::is_none"
    )]
    pub active_form: Option<String>,
}

/// The full set of work items for a session at a point in time.
///
/// `TodoWrite` is replace-all: every call carries the complete current list,
/// not a delta. So each snapshot wholly supersedes the previous one — the
/// latest `todo` event in the log is the session's current todo state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TodoSnapshot {
    pub todos: Vec<TodoItem>,
}

impl TodoSnapshot {
    /// Parse a `TodoWrite` tool input (`{ "todos": [ { content, status,
    /// activeForm } ] }`) into a normalized snapshot.
    ///
    /// Returns `None` when the input carries no `todos` array at all (e.g. the
    /// empty `{}` placeholder emitted at a streaming tool-block start), so
    /// callers can skip emitting a `todo` event for it. An explicitly empty
    /// `todos: []` is a valid replace-all (the agent cleared its list) and
    /// returns `Some` with an empty vec.
    pub fn from_todo_write_input(input: &serde_json::Value) -> Option<Self> {
        let arr = input.get("todos")?.as_array()?;
        let todos = arr
            .iter()
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
            .collect();
        Some(TodoSnapshot { todos })
    }
}

/// If a tool call is a `TodoWrite`, extract the normalized snapshot from it.
///
/// This is the single seam every provider uses to turn a raw tool invocation
/// into the canonical todo shape: the Claude process loop calls it on each
/// `tool_use`, and the mock provider calls it to stay byte-for-byte consistent
/// with the real parsing. Returns `None` for any other tool or a malformed
/// payload.
pub fn snapshot_from_tool_call(name: &str, input: &serde_json::Value) -> Option<TodoSnapshot> {
    if name != "TodoWrite" {
        return None;
    }
    TodoSnapshot::from_todo_write_input(input)
}

/// Stateful assembler turning Claude Code's task tools into [`TodoSnapshot`]s.
///
/// Claude Code ≥ 2.1 removed the replace-all `TodoWrite` tool in favor of an
/// incremental task list: `TaskCreate` adds one item (the assigned id only
/// appears in the tool *result*, as `tool_use_result.task.id`), and
/// `TaskUpdate` mutates one item by id (`pending` / `in_progress` /
/// `completed`, plus `deleted` to remove it). The tracker accumulates those
/// deltas per session and yields a full snapshot after every effective change,
/// so everything downstream (`todos` table mirror, WS broadcast, frontend)
/// keeps consuming the same replace-all `todo` events `TodoWrite` produced.
///
/// A `TodoWrite` call (older CLIs, mock provider) resets the tracker to that
/// snapshot, so both tool generations flow through one seam.
///
/// Mutations are applied at `ToolEnd` time, not `ToolStart`, because a
/// create's id lives in the result and a failed call must not change state.
#[derive(Debug, Default)]
pub struct TaskTracker {
    /// Tasks in creation order, keyed by the provider's task id.
    tasks: Vec<(String, TodoItem)>,
    /// Task tool calls seen at `ToolStart`, awaiting their `ToolEnd`.
    pending: std::collections::HashMap<String, PendingTaskCall>,
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

impl TaskTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed from a session's persisted todos (process respawn mid-
    /// conversation). The CLI assigns sequential ids starting at 1 and rows
    /// are stored in creation order, so `position + 1` reconstructs the ids
    /// a resumed conversation's `TaskUpdate` calls will reference.
    pub fn seed(todos: Vec<TodoItem>) -> Self {
        Self {
            tasks: todos
                .into_iter()
                .enumerate()
                .map(|(idx, item)| ((idx + 1).to_string(), item))
                .collect(),
            pending: std::collections::HashMap::new(),
        }
    }

    /// Feed a tool invocation. `TodoWrite` applies immediately (its input is
    /// the whole list) and returns the new snapshot; `TaskCreate` /
    /// `TaskUpdate` are parked until [`Self::on_tool_end`] confirms them.
    pub fn on_tool_start(
        &mut self,
        tool_use_id: &str,
        name: &str,
        input: &serde_json::Value,
    ) -> Option<TodoSnapshot> {
        match name {
            "TodoWrite" => {
                let snapshot = TodoSnapshot::from_todo_write_input(input)?;
                self.tasks = snapshot
                    .todos
                    .iter()
                    .enumerate()
                    .map(|(idx, item)| ((idx + 1).to_string(), item.clone()))
                    .collect();
                Some(snapshot)
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
    ) -> Option<TodoSnapshot> {
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

    fn snapshot(&self) -> TodoSnapshot {
        TodoSnapshot {
            todos: self.tasks.iter().map(|(_, item)| item.clone()).collect(),
        }
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

/// Task ids arrive as JSON strings today (`{"task":{"id":"1"}}`) but accept a
/// bare number too so a serialization change upstream doesn't drop captures.
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

    #[test]
    fn maps_provider_statuses_onto_lifecycle() {
        assert_eq!(TodoStatus::from_provider("pending"), TodoStatus::Pending);
        assert_eq!(
            TodoStatus::from_provider("in_progress"),
            TodoStatus::InProgress
        );
        assert_eq!(TodoStatus::from_provider("completed"), TodoStatus::Done);
        // Unknown tokens degrade to Pending, never dropped.
        assert_eq!(TodoStatus::from_provider("weird"), TodoStatus::Pending);
    }

    #[test]
    fn parses_todo_write_input_and_normalizes() {
        let input = serde_json::json!({
            "todos": [
                { "content": "Write code", "status": "completed", "activeForm": "Writing code" },
                { "content": "Run tests", "status": "in_progress", "activeForm": "Running tests" },
                { "content": "Ship", "status": "pending", "activeForm": "Shipping" },
            ]
        });
        let snap = TodoSnapshot::from_todo_write_input(&input).unwrap();
        assert_eq!(snap.todos.len(), 3);
        assert_eq!(snap.todos[0].status, TodoStatus::Done);
        assert_eq!(snap.todos[1].status, TodoStatus::InProgress);
        assert_eq!(snap.todos[2].status, TodoStatus::Pending);
        assert_eq!(snap.todos[1].active_form.as_deref(), Some("Running tests"));
    }

    #[test]
    fn missing_todos_array_yields_none_but_empty_is_some() {
        assert!(TodoSnapshot::from_todo_write_input(&serde_json::json!({})).is_none());
        let empty =
            TodoSnapshot::from_todo_write_input(&serde_json::json!({ "todos": [] })).unwrap();
        assert!(empty.todos.is_empty());
    }

    #[test]
    fn snapshot_only_matches_todo_write_tool() {
        let input = serde_json::json!({ "todos": [ { "content": "x", "status": "pending" } ] });
        assert!(snapshot_from_tool_call("Bash", &input).is_none());
        assert!(snapshot_from_tool_call("TodoWrite", &input).is_some());
    }

    fn create_result(id: &str) -> serde_json::Value {
        serde_json::json!({ "task": { "id": id, "subject": "x" } })
    }

    fn update_result() -> serde_json::Value {
        serde_json::json!({ "success": true })
    }

    #[test]
    fn task_tracker_assembles_create_and_update_deltas() {
        let mut t = TaskTracker::new();

        assert!(
            t.on_tool_start(
                "t1",
                "TaskCreate",
                &serde_json::json!({
                    "subject": "Write code",
                    "description": "details",
                    "activeForm": "Writing code",
                }),
            )
            .is_none(),
            "create applies at ToolEnd, not ToolStart"
        );
        let snap = t
            .on_tool_end("t1", false, Some(&create_result("1")))
            .unwrap();
        assert_eq!(snap.todos.len(), 1);
        assert_eq!(snap.todos[0].content, "Write code");
        assert_eq!(snap.todos[0].status, TodoStatus::Pending);
        assert_eq!(snap.todos[0].active_form.as_deref(), Some("Writing code"));

        t.on_tool_start(
            "t2",
            "TaskCreate",
            &serde_json::json!({ "subject": "Run tests", "description": "d" }),
        );
        let snap = t
            .on_tool_end("t2", false, Some(&create_result("2")))
            .unwrap();
        assert_eq!(snap.todos.len(), 2);

        t.on_tool_start(
            "t3",
            "TaskUpdate",
            &serde_json::json!({ "taskId": "1", "status": "in_progress" }),
        );
        let snap = t.on_tool_end("t3", false, Some(&update_result())).unwrap();
        assert_eq!(snap.todos[0].status, TodoStatus::InProgress);

        t.on_tool_start(
            "t4",
            "TaskUpdate",
            &serde_json::json!({ "taskId": "1", "status": "completed" }),
        );
        let snap = t.on_tool_end("t4", false, Some(&update_result())).unwrap();
        assert_eq!(snap.todos[0].status, TodoStatus::Done);
        assert_eq!(snap.todos[1].status, TodoStatus::Pending);
    }

    #[test]
    fn task_tracker_deleted_status_removes_the_task() {
        let mut t = TaskTracker::seed(vec![
            TodoItem {
                content: "a".into(),
                status: TodoStatus::Pending,
                active_form: None,
            },
            TodoItem {
                content: "b".into(),
                status: TodoStatus::Pending,
                active_form: None,
            },
        ]);
        t.on_tool_start(
            "t1",
            "TaskUpdate",
            &serde_json::json!({ "taskId": "1", "status": "deleted" }),
        );
        let snap = t.on_tool_end("t1", false, Some(&update_result())).unwrap();
        assert_eq!(snap.todos.len(), 1);
        assert_eq!(snap.todos[0].content, "b");
    }

    #[test]
    fn task_tracker_seed_aligns_ids_with_positions() {
        // A resumed conversation's TaskUpdate references the CLI's sequential
        // ids; seeding from persisted rows must reconstruct them.
        let mut t = TaskTracker::seed(vec![
            TodoItem {
                content: "first".into(),
                status: TodoStatus::Done,
                active_form: None,
            },
            TodoItem {
                content: "second".into(),
                status: TodoStatus::Pending,
                active_form: None,
            },
        ]);
        t.on_tool_start(
            "t1",
            "TaskUpdate",
            &serde_json::json!({ "taskId": "2", "status": "in_progress" }),
        );
        let snap = t.on_tool_end("t1", false, Some(&update_result())).unwrap();
        assert_eq!(snap.todos[1].status, TodoStatus::InProgress);
    }

    #[test]
    fn task_tracker_ignores_errors_unknown_ids_and_other_tools() {
        let mut t = TaskTracker::new();

        // Errored create must not change state.
        t.on_tool_start(
            "t1",
            "TaskCreate",
            &serde_json::json!({ "subject": "nope", "description": "d" }),
        );
        assert!(t.on_tool_end("t1", true, None).is_none());
        assert!(t.snapshot().todos.is_empty());

        // Update for an id we never saw is a no-op.
        t.on_tool_start(
            "t2",
            "TaskUpdate",
            &serde_json::json!({ "taskId": "99", "status": "completed" }),
        );
        assert!(t.on_tool_end("t2", false, Some(&update_result())).is_none());

        // Unrelated tools never produce snapshots.
        assert!(
            t.on_tool_start("t3", "Bash", &serde_json::json!({ "command": "ls" }))
                .is_none()
        );
        assert!(t.on_tool_end("t3", false, None).is_none());
    }

    #[test]
    fn task_tracker_create_falls_back_to_sequential_id_without_result() {
        let mut t = TaskTracker::new();
        t.on_tool_start(
            "t1",
            "TaskCreate",
            &serde_json::json!({ "subject": "a", "description": "d" }),
        );
        assert!(t.on_tool_end("t1", false, None).is_some());

        // The fallback id ("1") must be addressable by later updates.
        t.on_tool_start(
            "t2",
            "TaskUpdate",
            &serde_json::json!({ "taskId": "1", "status": "in_progress" }),
        );
        let snap = t.on_tool_end("t2", false, Some(&update_result())).unwrap();
        assert_eq!(snap.todos[0].status, TodoStatus::InProgress);
    }

    #[test]
    fn task_tracker_todo_write_resets_state() {
        let mut t = TaskTracker::new();
        t.on_tool_start(
            "t1",
            "TaskCreate",
            &serde_json::json!({ "subject": "old", "description": "d" }),
        );
        t.on_tool_end("t1", false, Some(&create_result("1")));

        let snap = t
            .on_tool_start(
                "t2",
                "TodoWrite",
                &serde_json::json!({
                    "todos": [
                        { "content": "new a", "status": "pending" },
                        { "content": "new b", "status": "in_progress" },
                    ]
                }),
            )
            .unwrap();
        assert_eq!(snap.todos.len(), 2);

        // Replace-all reassigned ids 1..N, so id "2" is "new b".
        t.on_tool_start(
            "t3",
            "TaskUpdate",
            &serde_json::json!({ "taskId": "2", "status": "completed" }),
        );
        let snap = t.on_tool_end("t3", false, Some(&update_result())).unwrap();
        assert_eq!(snap.todos[1].content, "new b");
        assert_eq!(snap.todos[1].status, TodoStatus::Done);
    }

    #[test]
    fn status_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(TodoStatus::InProgress).unwrap(),
            serde_json::json!("in_progress")
        );
        assert_eq!(TodoStatus::InProgress.label(), "In Progress");
    }
}
