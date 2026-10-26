//! Wire-compatible `ProviderEvent` (and friends) matching
//! `src/provider/stream.rs` serde (`tag = "kind", rename_all = "snake_case"`).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolImage {
    pub mime_type: String,
    pub data_base64: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrashKind {
    AuthExpired,
    RateLimit,
    Timeout,
    Interrupted,
    SpawnFailed,
    ResumeFailed,
    ExitedMidTurn,
    NoOutput,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
}

impl TodoStatus {
    pub fn from_provider(raw: &str) -> Self {
        match raw {
            "in_progress" => TodoStatus::InProgress,
            "completed" | "done" => TodoStatus::Done,
            _ => TodoStatus::Pending,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
    #[serde(
        default,
        rename = "activeForm",
        skip_serializing_if = "Option::is_none"
    )]
    pub active_form: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderEvent {
    Started {
        model: String,
        conversation_id: Option<String>,
        #[serde(default)]
        metadata: serde_json::Value,
    },
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolStart {
        tool_use_id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolEnd {
        tool_use_id: String,
        output: Option<String>,
        error: Option<String>,
        #[serde(default)]
        images: Vec<ToolImage>,
    },
    FileDiff {
        path: String,
        diff: String,
        added: i64,
        removed: i64,
        #[serde(default)]
        created: bool,
    },
    Todo {
        todos: Vec<TodoItem>,
    },
    Usage {
        input_tokens: i64,
        output_tokens: i64,
        cache_read_tokens: i64,
        cache_creation_tokens: i64,
        total_tokens: i64,
        context_tokens: i64,
        model: Option<String>,
        #[serde(default)]
        turn_seq: Option<i32>,
    },
    System {
        text: String,
        subtype: String,
        #[serde(default)]
        detail: serde_json::Value,
    },
    Completed {
        conversation_id: Option<String>,
        #[serde(default)]
        result_meta: serde_json::Value,
    },
    Crashed {
        reason: String,
        #[serde(default)]
        error_kind: CrashKind,
        exit_code: Option<i32>,
        stderr: Option<String>,
    },
    ControlRequest {
        request_id: String,
        request_type: String,
        payload: serde_json::Value,
    },
}
