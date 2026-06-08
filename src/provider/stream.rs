use serde::{Deserialize, Serialize};

use crate::todo::TodoItem;

/// Unified event stream from any AI provider.
/// Providers translate their native output format into these events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderEvent {
    /// Agent initialized / started running.
    Started {
        model: String,
        conversation_id: Option<String>,
        #[serde(default)]
        metadata: serde_json::Value,
    },
    /// Streamed text chunk.
    Text { text: String },
    /// Agent invoked a tool.
    ToolStart {
        tool_use_id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Tool finished.
    ToolEnd {
        tool_use_id: String,
        output: Option<String>,
        error: Option<String>,
    },
    /// The agent reported its current todo list (a full replace-all snapshot
    /// of its trackable work items). Provider-agnostic — any provider that can
    /// surface work items emits this; the latest one wins.
    Todo { todos: Vec<TodoItem> },
    /// Agent finished normally.
    Completed { conversation_id: Option<String> },
    /// Agent failed / crashed.
    Crashed {
        reason: String,
        exit_code: Option<i32>,
        stderr: Option<String>,
    },
    /// Agent requesting permission or user input.
    ControlRequest {
        request_id: String,
        request_type: String,
        payload: serde_json::Value,
    },
}

impl ProviderEvent {
    /// Map this provider event to an event log kind string.
    pub fn event_kind(&self) -> &'static str {
        match self {
            ProviderEvent::Started { .. } => "agent-start",
            ProviderEvent::Text { .. } => "agent-text",
            ProviderEvent::ToolStart { .. } => "agent-tool-start",
            ProviderEvent::ToolEnd { .. } => "agent-tool-end",
            ProviderEvent::Todo { .. } => "todo",
            ProviderEvent::Completed { .. } => "agent-end",
            ProviderEvent::Crashed { .. } => "agent-end",
            ProviderEvent::ControlRequest { .. } => "question",
        }
    }

    /// Convert to event log data JSON.
    pub fn event_data(&self) -> serde_json::Value {
        match self {
            ProviderEvent::Started {
                model,
                conversation_id,
                metadata,
            } => serde_json::json!({
                "model": model,
                "conversationId": conversation_id,
                "metadata": metadata,
            }),
            ProviderEvent::Text { text } => serde_json::json!({ "text": text }),
            ProviderEvent::ToolStart {
                tool_use_id,
                name,
                input,
            } => serde_json::json!({
                "toolUseId": tool_use_id,
                "name": name,
                "input": input,
            }),
            ProviderEvent::ToolEnd {
                tool_use_id,
                output,
                error,
            } => serde_json::json!({
                "toolUseId": tool_use_id,
                "output": output,
                "error": error,
            }),
            ProviderEvent::Todo { todos } => serde_json::json!({ "todos": todos }),
            ProviderEvent::Completed { conversation_id } => serde_json::json!({
                "status": "complete",
                "conversationId": conversation_id,
            }),
            ProviderEvent::Crashed {
                reason,
                exit_code,
                stderr,
            } => serde_json::json!({
                "status": "crashed",
                "reason": reason,
                "exitCode": exit_code,
                "stderr": stderr,
            }),
            ProviderEvent::ControlRequest {
                request_id,
                request_type,
                payload,
            } => serde_json::json!({
                "requestId": request_id,
                "requestType": request_type,
                "payload": payload,
            }),
        }
    }
}

/// Configuration for spawning an agent run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnConfig {
    pub model: String,
    pub effort: Option<String>,
    pub working_dir: String,
    pub mcp_config_path: Option<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    pub permission_mode: Option<String>,
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// Model info from a provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_kind_mapping() {
        assert_eq!(
            ProviderEvent::Started {
                model: "opus".into(),
                conversation_id: None,
                metadata: serde_json::Value::Null,
            }
            .event_kind(),
            "agent-start"
        );
        assert_eq!(
            ProviderEvent::Text { text: "hi".into() }.event_kind(),
            "agent-text"
        );
        assert_eq!(
            ProviderEvent::Completed {
                conversation_id: None
            }
            .event_kind(),
            "agent-end"
        );
        assert_eq!(
            ProviderEvent::Crashed {
                reason: "oops".into(),
                exit_code: Some(1),
                stderr: None,
            }
            .event_kind(),
            "agent-end"
        );
    }

    #[test]
    fn test_event_data_serialization() {
        let event = ProviderEvent::Text {
            text: "hello".into(),
        };
        let data = event.event_data();
        assert_eq!(data["text"], "hello");

        let event = ProviderEvent::Crashed {
            reason: "timeout".into(),
            exit_code: Some(137),
            stderr: Some("killed".into()),
        };
        let data = event.event_data();
        assert_eq!(data["status"], "crashed");
        assert_eq!(data["reason"], "timeout");
        assert_eq!(data["exitCode"], 137);
    }
}
