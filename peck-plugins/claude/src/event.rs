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
impl CrashKind {
    /// Stable wire string — identical to the serde representation and to
    /// core's `src/provider/stream.rs::CrashKind::as_str`.
    pub fn as_str(self) -> &'static str {
        match self {
            CrashKind::AuthExpired => "auth_expired",
            CrashKind::RateLimit => "rate_limit",
            CrashKind::Timeout => "timeout",
            CrashKind::Interrupted => "interrupted",
            CrashKind::SpawnFailed => "spawn_failed",
            CrashKind::ResumeFailed => "resume_failed",
            CrashKind::ExitedMidTurn => "exited_mid_turn",
            CrashKind::NoOutput => "no_output",
            CrashKind::Unknown => "unknown",
        }
    }

    /// Best-effort classification of free-form provider text — a stderr
    /// tail, an API error body, a CLI message. Keep the needle lists in
    /// sync with core's `CrashKind::classify`; the host only reclassifies
    /// `Unknown`, so the plugin must sort resume/auth/rate-limit failures
    /// itself for resume_recovery and auth-recovery to fire.
    pub fn classify(text: &str) -> CrashKind {
        let text = text.to_ascii_lowercase();
        let has = |needles: &[&str]| needles.iter().any(|n| text.contains(n));
        // Resume rejections first: the CLI words one as a failure to start
        // ("no conversation found"), which the auth bucket below would
        // otherwise swallow — and prescribe the wrong remedy.
        if has(&[
            "no rollout found",
            "thread/resume",
            "no conversation found",
            "no session found",
            "session not found",
            "conversation not found",
        ]) {
            return CrashKind::ResumeFailed;
        }
        // Rate limiting next: a 429 body often also names the API key.
        if has(&[
            "429",
            "rate limit",
            "rate_limit",
            "too many requests",
            "quota",
            "usage limit",
            "overloaded",
        ]) {
            return CrashKind::RateLimit;
        }
        if has(&[
            "401",
            "unauthorized",
            "unauthenticated",
            "authenticate",
            "authentication",
            "invalid api key",
            "invalid_api_key",
            "api key",
            "credential",
            "not signed in",
            "isn't signed in",
            "no model configured",
            "login",
            "oauth",
            "token expired",
            "expired token",
        ]) {
            return CrashKind::AuthExpired;
        }
        if has(&["timed out", "timeout"]) {
            return CrashKind::Timeout;
        }
        if has(&["interrupted", "cancelled", "canceled"]) {
            return CrashKind::Interrupted;
        }
        CrashKind::Unknown
    }

    /// [`classify`](Self::classify), falling back to a structural kind when
    /// the text carries nothing recognizable.
    pub fn classify_or(text: &str, fallback: CrashKind) -> CrashKind {
        match CrashKind::classify(text) {
            CrashKind::Unknown => fallback,
            kind => kind,
        }
    }
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
        /// `tool_use_id` of the built-in Task/Agent subagent call this
        /// frame belongs to; `None` at top level.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_tool_use_id: Option<String>,
    },
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_tool_use_id: Option<String>,
    },
    ToolStart {
        tool_use_id: String,
        name: String,
        input: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_tool_use_id: Option<String>,
    },
    ToolEnd {
        tool_use_id: String,
        output: Option<String>,
        error: Option<String>,
        #[serde(default)]
        images: Vec<ToolImage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_tool_use_id: Option<String>,
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
