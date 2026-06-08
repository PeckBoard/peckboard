//! Canonical representation of a trackable work item — a "todo"/"task".
//!
//! Peckboard treats todos and tasks as one unified concept: a single work
//! item with a three-state lifecycle, `Pending → In Progress → Done`. There is
//! no separate "task" type; everywhere in the codebase a trackable unit of
//! work is a [`TodoItem`] in this lifecycle.
//!
//! The shape here is deliberately provider-agnostic. Claude surfaces its list
//! via the `TodoWrite` tool (statuses `pending` | `in_progress` | `completed`),
//! but a third-party Extism provider could report work items differently. Both
//! normalize onto [`TodoStatus`] so downstream consumers (routes, frontend)
//! never need to know which agent produced the snapshot.

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

    #[test]
    fn status_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(TodoStatus::InProgress).unwrap(),
            serde_json::json!("in_progress")
        );
        assert_eq!(TodoStatus::InProgress.label(), "In Progress");
    }
}
