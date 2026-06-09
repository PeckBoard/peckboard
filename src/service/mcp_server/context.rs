//! Per-tool-call context plumbed into every MCP handler, plus the scope-
//! verification proof token and the inter-worker rate limiter.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::Db;

/// Best-effort seam for dispatching a live capture run on an expert
/// session. The MCP tool layer has no access to the `SessionManager` /
/// token registry / data dir, so `spin_up_experts` can't spawn agents on
/// its own. The production impl (wired in the `mcp` route from
/// `AppState`, see `super::spawn::AppExpertDispatcher`) issues an MCP
/// token, writes the per-session config, and dispatches a capture run via
/// `SessionManager::send_message_locked`. Kept as a narrow trait so the
/// tool context depends only on this capability, not the whole `AppState`.
pub trait ExpertDispatcher: Send + Sync {
    fn dispatch_capture<'a>(
        &'a self,
        expert_session_id: &'a str,
        prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;
}

/// Context scoped from the MCP token — identifies what session/project/card
/// the tool call is operating within.
#[derive(Clone)]
pub struct ToolCallContext {
    pub session_id: String,
    pub project_id: Option<String>,
    pub card_id: Option<String>,
    pub db: Arc<Db>,
    pub broadcaster: Arc<crate::ws::broadcaster::Broadcaster>,
    pub provider_registry: Option<Arc<crate::provider::registry::ProviderRegistry>>,
    /// Present only on real tool calls from the `mcp` route; `None` in unit
    /// tests and contexts without a running app. When `None`, expert tools
    /// still create + persist experts and their captured knowledge but skip
    /// the live agent dispatch.
    pub expert_dispatcher: Option<Arc<dyn ExpertDispatcher>>,
}

/// Proof token: a project id verified against the current MCP token's
/// scope. The only way to obtain one is via `ToolCallContext::scope_project`,
/// `scope_card`, or `scope_session` — each compares the target project
/// against `ctx.project_id` and returns `Forbidden` on mismatch.
///
/// Every MCP handler that touches a project- or card-scoped resource
/// MUST start by calling one of those helpers, then use the returned
/// `ScopedProjectId` for downstream DB calls. The bug class fixed by this
/// type: prior to the proof-token rollout, only three handlers
/// (update/pause/resume_project) were scope-checked at the route layer.
/// `handle_create_card` and ~10 other handlers happily accepted
/// `args.project_id` raw, so a worker scoped to project A could create
/// cards, complete steps, share findings, etc. inside project B by
/// passing a different `project_id` in the arguments.
#[derive(Debug, Clone)]
pub struct ScopedProjectId(String);

impl ScopedProjectId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl ToolCallContext {
    /// Verify the given target project_id is allowed by the current MCP
    /// token's scope. Resolution:
    /// * token scoped, target provided → must match;
    /// * token scoped, target absent → returns the token's project;
    /// * token unscoped, target provided → accepted as-is;
    /// * token unscoped, target absent → error ("project_id required").
    pub fn scope_project(&self, target: Option<&str>) -> anyhow::Result<ScopedProjectId> {
        match (target, self.project_id.as_deref()) {
            (Some(t), Some(scoped)) if t != scoped => {
                anyhow::bail!("token scoped to project {scoped}, cannot target {t}")
            }
            (Some(t), _) => Ok(ScopedProjectId(t.to_string())),
            (None, Some(scoped)) => Ok(ScopedProjectId(scoped.to_string())),
            (None, None) => anyhow::bail!("project_id required"),
        }
    }

    /// Resolve the card's project_id, then scope-check it.
    pub async fn scope_card(&self, card_id: &str) -> anyhow::Result<ScopedProjectId> {
        let card = self
            .db
            .get_card(card_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("card not found: {card_id}"))?;
        self.scope_project(Some(&card.project_id))
    }

    /// Resolve the session's project_id, then scope-check it. Used by
    /// tools like `send_worker_message` that target a peer worker session.
    pub async fn scope_session(&self, session_id: &str) -> anyhow::Result<ScopedProjectId> {
        let session = self
            .db
            .get_session(session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("session not found: {session_id}"))?;
        let project_id = session
            .project_id
            .ok_or_else(|| anyhow::anyhow!("session is not part of any project: {session_id}"))?;
        self.scope_project(Some(&project_id))
    }
}

/// A single MCP tool definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Rate limiter for inter-worker communication per project.
pub(super) struct CommRateLimiter {
    /// (project_id, window_start) -> count
    counts: std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, u32)>>,
    pub(super) max_per_window: u32,
    pub(super) window_secs: u64,
}

impl CommRateLimiter {
    pub(super) fn new(max_per_window: u32, window_secs: u64) -> Self {
        CommRateLimiter {
            counts: std::sync::Mutex::new(std::collections::HashMap::new()),
            max_per_window,
            window_secs,
        }
    }

    /// Check if a call is allowed. Returns Ok(remaining) or Err(seconds_until_reset).
    pub(super) fn check(&self, project_id: &str) -> Result<u32, u64> {
        let mut counts = self.counts.lock().unwrap();
        let now = std::time::Instant::now();
        let window = std::time::Duration::from_secs(self.window_secs);

        let entry = counts.entry(project_id.to_string()).or_insert((now, 0));
        if now.duration_since(entry.0) >= window {
            // Reset window
            *entry = (now, 1);
            Ok(self.max_per_window - 1)
        } else if entry.1 < self.max_per_window {
            entry.1 += 1;
            Ok(self.max_per_window - entry.1)
        } else {
            let reset_in = self.window_secs - now.duration_since(entry.0).as_secs();
            Err(reset_in)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_for_scope(project_id: Option<&str>) -> ToolCallContext {
        ToolCallContext {
            session_id: "s1".into(),
            project_id: project_id.map(|s| s.to_string()),
            card_id: None,
            db: Arc::new(crate::db::Db::in_memory().unwrap()),
            broadcaster: crate::ws::broadcaster::Broadcaster::new(),
            provider_registry: None,
            expert_dispatcher: None,
        }
    }

    #[test]
    fn scope_project_unscoped_token_passes_target_through() {
        let ctx = ctx_for_scope(None);
        let s = ctx.scope_project(Some("p-1")).unwrap();
        assert_eq!(s.as_str(), "p-1");
    }

    #[test]
    fn scope_project_scoped_token_accepts_matching_target() {
        let ctx = ctx_for_scope(Some("p-1"));
        let s = ctx.scope_project(Some("p-1")).unwrap();
        assert_eq!(s.as_str(), "p-1");
    }

    #[test]
    fn scope_project_scoped_token_rejects_mismatched_target() {
        let ctx = ctx_for_scope(Some("p-1"));
        let err = ctx.scope_project(Some("p-2")).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("p-1") && msg.contains("p-2"),
            "expected scope-mismatch error to mention both project ids, got: {msg}"
        );
    }

    #[test]
    fn scope_project_scoped_token_no_target_returns_token_scope() {
        let ctx = ctx_for_scope(Some("p-1"));
        let s = ctx.scope_project(None).unwrap();
        assert_eq!(s.as_str(), "p-1");
    }

    #[test]
    fn scope_project_unscoped_token_no_target_errors() {
        let ctx = ctx_for_scope(None);
        let err = ctx.scope_project(None).unwrap_err();
        assert!(err.to_string().contains("project_id required"));
    }
}
