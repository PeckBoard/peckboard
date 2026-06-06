use serde::{Deserialize, Serialize};

/// Verdict returned by a plugin for a hook call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    /// Allow the operation to proceed, optionally with a modified payload.
    Allow {
        #[serde(skip_serializing_if = "Option::is_none")]
        payload: Option<serde_json::Value>,
    },
    /// Cancel the operation with a reason.
    Cancel { reason: String },
    /// This plugin has no opinion — pass through unchanged.
    Skip,
}

impl Verdict {
    pub fn allow() -> Self {
        Verdict::Allow { payload: None }
    }

    pub fn allow_modified(payload: serde_json::Value) -> Self {
        Verdict::Allow {
            payload: Some(payload),
        }
    }

    pub fn cancel(reason: impl Into<String>) -> Self {
        Verdict::Cancel {
            reason: reason.into(),
        }
    }

    pub fn skip() -> Self {
        Verdict::Skip
    }
}

/// Result of dispatching a hook to all registered plugins.
#[derive(Debug)]
pub enum HookResult {
    /// All plugins allowed (or skipped). Contains the final payload (possibly modified).
    Allowed(serde_json::Value),
    /// A plugin cancelled the operation.
    Cancelled { plugin: String, reason: String },
}

impl HookResult {
    pub fn is_cancelled(&self) -> bool {
        matches!(self, HookResult::Cancelled { .. })
    }

    pub fn into_payload(self) -> Option<serde_json::Value> {
        match self {
            HookResult::Allowed(v) => Some(v),
            HookResult::Cancelled { .. } => None,
        }
    }
}

/// Plugin manifest declaring which hooks a plugin handles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub hooks: Vec<String>,
    #[serde(default)]
    pub http_routes: Vec<String>,
}
