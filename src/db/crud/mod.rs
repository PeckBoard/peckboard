//! Inherent `impl Db` blocks for each entity, grouped one module per entity.
//! All methods land on `Db` regardless of which submodule defines them.

mod announcements;
mod auth_sessions;
mod cards;
mod cascades;
mod claude_accounts;
mod dependencies;
mod env_vars;
mod events;
mod folders;
mod grok_accounts;
mod kimi_accounts;
mod plans;
mod plugin_approvals;
mod plugin_data;
mod plugin_repositories;
mod plugin_settings;
mod projects;
mod push;
mod queued;
mod repeating_tasks;
mod sessions;
mod system_prompts;
mod tabs;
mod todos;
mod usage;
mod users;
mod workflow_instructions;

pub use claude_accounts::AccountModelUsage;
pub use folders::{MoveFolderOutcome, ProjectMoveReport, RepeatingTaskMoveReport};
pub use plugin_approvals::{APPROVAL_APPROVED, APPROVAL_DENIED};
pub use todos::ProjectCardTodos;
pub use usage::UsageRollupRow;

/// Outcome of `Db::delete_folder_if_empty`. Avoids the older check-then-
/// act pattern (`list_sessions_by_folder().await` + `delete_folder().await`)
/// where a concurrent session creation could slip in between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderEmptyDelete {
    Deleted,
    HasSessions(usize),
    NotFound,
}

/// Summary of what a cascade-delete operation actually removed. Used so
/// the caller can log / surface "deleted N cards, M sessions, K events"
/// without re-querying.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CascadeReport {
    pub sessions_deleted: usize,
    pub events_deleted: usize,
    pub cards_deleted: usize,
}
