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

    /// Resume a session with `text` exactly as an incoming user message would:
    /// hand off to `SessionManager::send_or_queue`, which spawns a fresh agent
    /// run when the session is idle, or queues / injects mid-stream when it is
    /// already running. The caller is responsible for having already persisted
    /// the matching `user` event; this only drives the agent. A no-op seam in
    /// headless/test contexts (there is no dispatcher to call), which is why
    /// persistence is kept separate.
    fn resume_session<'a>(
        &'a self,
        session_id: &'a str,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;
}

/// Context scoped from the MCP token — identifies what session/project/card
/// the tool call is operating within.
#[derive(Clone)]
pub struct ToolCallContext {
    pub session_id: String,
    pub project_id: Option<String>,
    pub card_id: Option<String>,
    /// Folder the caller's session belongs to. Every session has a folder,
    /// so this is non-optional. Resolved at the `mcp` route layer from the
    /// session row tied to the bearer token; in unit tests it is whatever
    /// folder the test put the session in. Used by `scope_project` /
    /// `scope_card` / `scope_session` to enforce the folder boundary, and
    /// by every listing tool that needs to filter to "things in MY folder
    /// or globally visible".
    pub folder_id: String,
    pub db: Arc<Db>,
    pub broadcaster: Arc<crate::ws::broadcaster::Broadcaster>,
    pub provider_registry: Option<Arc<crate::provider::registry::ProviderRegistry>>,
    /// Data dir for durable exports. `Some` on real tool calls from the `mcp`
    /// route; when `None`, handlers skip the export hook.
    pub data_dir: Option<std::path::PathBuf>,
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

/// Proof token: a folder id obtained by resolving the **current session's**
/// folder, with the additional guarantee that the session is *not* part
/// of a project. Repeating-task MCP tools take a `&ScopedFolderId` so
/// they can never be used from a worker session to reach across into a
/// folder the worker isn't supposed to touch.
///
/// The only way to construct one is [`ToolCallContext::scope_folder`],
/// which returns Err if `ctx.project_id` is set or if the underlying
/// session is itself bound to a project.
#[derive(Debug, Clone)]
pub struct ScopedFolderId(String);

impl ScopedFolderId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Proof token: bearer has verified that some target folder id equals the
/// caller's own folder (`ctx.folder_id`). The only way to obtain one is
/// [`ToolCallContext::scope_folder_target`], which compares the supplied
/// folder id against the caller's folder and returns Err on mismatch.
/// Used by MCP tools that take an explicit `folder_id` argument
/// (`create_project`, the folder-change endpoints) so the type system
/// enforces "the folder you're acting on is the one you're allowed to act
/// in" — no caller can pass an arbitrary folder id and have downstream
/// code trust it.
#[derive(Debug, Clone)]
pub struct ScopedFolderTarget(String);

impl ScopedFolderTarget {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ToolCallContext {
    /// Verify the given target project_id is allowed by the current MCP
    /// token's scope AND lives in the caller's folder. Two independent
    /// boundaries are enforced:
    ///
    /// 1. Project-scope: a worker token (`project_id` set) refuses any
    ///    target project that doesn't match the token's project.
    /// 2. Folder-scope: every resolved target project must live in
    ///    `self.folder_id`. This blocks a chat session (unscoped token)
    ///    from reaching projects in sibling folders, and is defence-in-
    ///    depth for worker tokens (a token whose project moved to another
    ///    folder mid-call rejects rather than acts on stale state).
    ///
    /// Resolution:
    /// * token scoped, target provided → must match the token's project,
    ///   then folder-checked;
    /// * token scoped, target absent → returns the token's project,
    ///   folder-checked;
    /// * token unscoped, target provided → folder-checked;
    /// * token unscoped, target absent → error ("project_id required").
    pub async fn scope_project(&self, target: Option<&str>) -> anyhow::Result<ScopedProjectId> {
        let project_id = match (target, self.project_id.as_deref()) {
            (Some(t), Some(scoped)) if t != scoped => {
                // Use "not found" framing so a worker on project A can't
                // probe for project B's existence by id-guessing — the
                // 404-vs-403 distinction the PM expert flagged.
                anyhow::bail!("project not found: {t}")
            }
            (Some(t), _) => t.to_string(),
            (None, Some(scoped)) => scoped.to_string(),
            (None, None) => anyhow::bail!("project_id required"),
        };

        // Confirm the target project lives in the caller's folder. The
        // lookup also doubles as an existence check so a bogus id returns
        // the same "not found" the cross-folder branch does — no
        // existence leak.
        let project = self
            .db
            .get_project(&project_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("project not found: {project_id}"))?;
        if project.folder_id != self.folder_id {
            anyhow::bail!("project not found: {project_id}");
        }

        Ok(ScopedProjectId(project_id))
    }

    /// Resolve the card's project_id, then scope-check it. The card's
    /// folder is inherited from its project; `scope_project` enforces the
    /// folder boundary there.
    pub async fn scope_card(&self, card_id: &str) -> anyhow::Result<ScopedProjectId> {
        let card = self
            .db
            .get_card(card_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("card not found: {card_id}"))?;
        self.scope_project(Some(&card.project_id)).await
    }

    /// Resolve the session's project_id, then scope-check it. Used by
    /// tools like `send_worker_message` that target a peer worker session.
    /// The target session must also be in the caller's folder — a worker
    /// in folder A can't message a session in folder B even if the bare
    /// session_id is guessed.
    pub async fn scope_session(&self, session_id: &str) -> anyhow::Result<ScopedProjectId> {
        let session = self
            .db
            .get_session(session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("session not found: {session_id}"))?;
        // Folder-check first: leak nothing about a foreign-folder session.
        if session.folder_id != self.folder_id {
            anyhow::bail!("session not found: {session_id}");
        }
        let project_id = session
            .project_id
            .ok_or_else(|| anyhow::anyhow!("session is not part of any project: {session_id}"))?;
        self.scope_project(Some(&project_id)).await
    }

    /// Resolve the current session's folder for the repeating-task MCP
    /// tools. Refuses sessions that are part of a project — those tools
    /// are intentionally only available to plain (non-worker) sessions
    /// so a worker can't reach across folders via the task table.
    pub async fn scope_folder(&self) -> anyhow::Result<ScopedFolderId> {
        // Defence in depth: token scope already excludes folder-CRUD for
        // a worker token (project_id would be Some), but check both
        // the token AND the persisted session row so a stale MCP config
        // hand-edited to drop the project_id scope still gets rejected.
        if self.project_id.is_some() {
            anyhow::bail!("repeating-task tools are not available in worker sessions");
        }
        let session = self
            .db
            .get_session(&self.session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("session not found: {}", self.session_id))?;
        if session.project_id.is_some() {
            anyhow::bail!("repeating-task tools are not available in worker sessions");
        }
        Ok(ScopedFolderId(session.folder_id))
    }

    /// Verify that an explicit target folder id matches the caller's own
    /// folder. Used by tools that take a `folder_id` argument
    /// (`create_project`, the folder-change endpoints) so passing a
    /// foreign folder id fails at the boundary instead of silently
    /// creating cross-folder rows. Same "not found" framing as the
    /// project / session scope checks: a caller can't use this to probe
    /// for the existence of sibling folders.
    pub fn scope_folder_target(&self, target: &str) -> anyhow::Result<ScopedFolderTarget> {
        if target != self.folder_id {
            anyhow::bail!("folder not found: {target}");
        }
        Ok(ScopedFolderTarget(target.to_string()))
    }

    /// The caller's own folder, wrapped in the proof token. Convenience
    /// for tools that default to "act in my folder" when no explicit
    /// folder is supplied.
    pub fn caller_folder(&self) -> ScopedFolderTarget {
        ScopedFolderTarget(self.folder_id.clone())
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

    async fn seed_folder_and_project(db: &crate::db::Db, folder_id: &str, project_id: &str) {
        use crate::db::models::{NewFolder, NewProject};
        let ts = chrono::Utc::now().to_rfc3339();
        let _ = db
            .create_folder(NewFolder {
                id: folder_id.into(),
                name: folder_id.into(),
                path: format!("/tmp/{folder_id}"),
                created_at: ts.clone(),
            })
            .await;
        let _ = db
            .create_project(NewProject {
                id: project_id.into(),
                name: project_id.into(),
                context: "".into(),
                folder_id: folder_id.into(),
                worker_count: 1,
                status: "active".into(),
                workflow: "task".into(),
                model: None,
                effort: None,
                parallel_instructions: false,
                auto_notify_changes: true,
                worker_communication: false,
                created_at: ts.clone(),
                budget_usd_cents: None,
                budget_period: None,
                last_accessed_at: ts.clone(),
                worktree_isolation: false,
            })
            .await;
    }

    async fn ctx_for_scope(project_id: Option<&str>) -> ToolCallContext {
        let db = Arc::new(crate::db::Db::in_memory().unwrap());
        seed_folder_and_project(&db, "f-1", "p-1").await;
        ToolCallContext {
            session_id: "s1".into(),
            project_id: project_id.map(|s| s.to_string()),
            card_id: None,
            folder_id: "f-1".into(),
            db,
            broadcaster: crate::ws::broadcaster::Broadcaster::new(),
            provider_registry: None,
            data_dir: None,
        }
    }

    #[tokio::test]
    async fn scope_project_unscoped_token_passes_target_through() {
        let ctx = ctx_for_scope(None).await;
        let s = ctx.scope_project(Some("p-1")).await.unwrap();
        assert_eq!(s.as_str(), "p-1");
    }

    #[tokio::test]
    async fn scope_project_scoped_token_accepts_matching_target() {
        let ctx = ctx_for_scope(Some("p-1")).await;
        let s = ctx.scope_project(Some("p-1")).await.unwrap();
        assert_eq!(s.as_str(), "p-1");
    }

    #[tokio::test]
    async fn scope_project_scoped_token_rejects_mismatched_target() {
        let ctx = ctx_for_scope(Some("p-1")).await;
        let err = ctx.scope_project(Some("p-2")).await.unwrap_err();
        let msg = err.to_string();
        // "not found" framing avoids existence leaks across projects.
        assert!(msg.contains("not found"), "got: {msg}");
        assert!(msg.contains("p-2"), "got: {msg}");
    }

    #[tokio::test]
    async fn scope_project_scoped_token_no_target_returns_token_scope() {
        let ctx = ctx_for_scope(Some("p-1")).await;
        let s = ctx.scope_project(None).await.unwrap();
        assert_eq!(s.as_str(), "p-1");
    }

    #[tokio::test]
    async fn scope_project_unscoped_token_no_target_errors() {
        let ctx = ctx_for_scope(None).await;
        let err = ctx.scope_project(None).await.unwrap_err();
        assert!(err.to_string().contains("project_id required"));
    }

    #[tokio::test]
    async fn scope_project_rejects_project_in_a_different_folder() {
        // Caller is in folder f-1. Target project p-2 exists but lives in f-2.
        let db = Arc::new(crate::db::Db::in_memory().unwrap());
        seed_folder_and_project(&db, "f-1", "p-1").await;
        seed_folder_and_project(&db, "f-2", "p-2").await;
        let ctx = ToolCallContext {
            session_id: "s1".into(),
            project_id: None, // chat session
            card_id: None,
            folder_id: "f-1".into(),
            db,
            broadcaster: crate::ws::broadcaster::Broadcaster::new(),
            provider_registry: None,
            data_dir: None,
        };
        let err = ctx.scope_project(Some("p-2")).await.unwrap_err();
        // "not found", not "forbidden" — don't leak the project's existence.
        assert!(err.to_string().contains("not found"), "got: {err}");
    }

    #[tokio::test]
    async fn scope_folder_target_accepts_caller_folder() {
        let ctx = ctx_for_scope(None).await;
        let scope = ctx.scope_folder_target("f-1").unwrap();
        assert_eq!(scope.as_str(), "f-1");
    }

    #[tokio::test]
    async fn scope_folder_target_rejects_foreign_folder() {
        let ctx = ctx_for_scope(None).await;
        let err = ctx.scope_folder_target("f-2").unwrap_err();
        assert!(err.to_string().contains("not found"), "got: {err}");
    }

    // ── ScopedFolderId: repeating-task scope guard ───────────────────────

    async fn seed_session(
        db: &crate::db::Db,
        session_id: &str,
        folder_id: &str,
        project_id: Option<&str>,
    ) {
        use crate::db::models::{NewFolder, NewSession};
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: folder_id.into(),
            name: folder_id.into(),
            path: format!("/tmp/{folder_id}"),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_session(NewSession {
            id: session_id.into(),
            name: session_id.into(),
            folder_id: folder_id.into(),
            model: None,
            effort: None,
            is_worker: false,
            project_id: project_id.map(String::from),
            card_id: None,
            conversation_id: None,
            created_at: ts.clone(),
            last_activity: ts,
            repeating_task_id: None,
            ..Default::default()
        })
        .await
        .unwrap();
    }

    fn ctx_for_session(
        db: Arc<crate::db::Db>,
        session_id: &str,
        project_id: Option<&str>,
    ) -> ToolCallContext {
        ToolCallContext {
            session_id: session_id.to_string(),
            project_id: project_id.map(String::from),
            card_id: None,
            folder_id: "f1".into(),
            db,
            broadcaster: crate::ws::broadcaster::Broadcaster::new(),
            provider_registry: None,
            data_dir: None,
        }
    }

    #[tokio::test]
    async fn scope_folder_returns_session_folder_for_plain_session() {
        let db = Arc::new(crate::db::Db::in_memory().unwrap());
        seed_session(&db, "s1", "f1", None).await;
        let ctx = ctx_for_session(db, "s1", None);
        let scope = ctx.scope_folder().await.unwrap();
        assert_eq!(scope.as_str(), "f1");
    }

    #[tokio::test]
    async fn scope_folder_rejects_token_with_project_scope() {
        // Token scope explicitly tagged with a project_id — even if the
        // session row itself is a plain session, the token says "you are
        // operating inside project P", which means repeating-task tools
        // must be unavailable.
        let db = Arc::new(crate::db::Db::in_memory().unwrap());
        seed_session(&db, "s1", "f1", None).await;
        let ctx = ctx_for_session(db, "s1", Some("p1"));
        let err = ctx.scope_folder().await.unwrap_err();
        assert!(err.to_string().contains("worker sessions"), "got: {err}",);
    }

    #[tokio::test]
    async fn scope_folder_rejects_session_bound_to_project() {
        // Token scope is unscoped, but the persisted session has a
        // project_id (worker session). Still rejected — defence in depth.
        let db = Arc::new(crate::db::Db::in_memory().unwrap());
        use crate::db::models::{NewFolder, NewProject, NewSession};
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "f1".into(),
            path: "/tmp/f1".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_project(NewProject {
            id: "p1".into(),
            name: "P".into(),
            context: "".into(),
            folder_id: "f1".into(),
            worker_count: 1,
            status: "active".into(),
            workflow: "task".into(),
            model: None,
            effort: None,
            parallel_instructions: false,
            auto_notify_changes: true,
            worker_communication: false,
            created_at: ts.clone(),
            budget_usd_cents: None,
            budget_period: None,
            last_accessed_at: ts.clone(),
            worktree_isolation: false,
        })
        .await
        .unwrap();
        db.create_session(NewSession {
            id: "s1".into(),
            name: "worker".into(),
            folder_id: "f1".into(),
            model: None,
            effort: None,
            is_worker: true,
            project_id: Some("p1".into()),
            card_id: None,
            conversation_id: None,
            created_at: ts.clone(),
            last_activity: ts,
            repeating_task_id: None,
            ..Default::default()
        })
        .await
        .unwrap();
        let ctx = ctx_for_session(db, "s1", None);
        let err = ctx.scope_folder().await.unwrap_err();
        assert!(err.to_string().contains("worker sessions"), "got: {err}");
    }

    #[tokio::test]
    async fn scope_folder_rejects_unknown_session() {
        let db = Arc::new(crate::db::Db::in_memory().unwrap());
        let ctx = ctx_for_session(db, "nope", None);
        assert!(ctx.scope_folder().await.is_err());
    }
}
