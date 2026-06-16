//! Inherent `impl Db` blocks for each entity, grouped one module per entity.
//! All methods land on `Db` regardless of which submodule defines them.

mod announcements;
mod auth_sessions;
mod cards;
mod cascades;
mod dependencies;
mod events;
mod folders;
mod plugin_approvals;
mod plugin_settings;
mod pm_decisions;
mod projects;
mod push;
mod queued;
mod repeating_tasks;
mod sessions;
mod tabs;
mod todos;
mod usage;
mod users;
mod workflow_instructions;

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
