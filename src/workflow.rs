//! Canonical workflow definitions — the single source of truth for the ordered
//! steps a card moves through.
//!
//! A card selects a workflow by id: its own `workflow` field, falling back to
//! the project's `default_workflow`, falling back to [`DEFAULT_WORKFLOW_ID`].
//! An unknown or missing id resolves to the default workflow rather than an
//! empty list, so `complete_step` can always find the next step (an empty list
//! would strand the card or jump it straight to `done`).
//!
//! Everything that needs step order — the worker prompt, `complete_step`
//! advancement in the orchestrator, the HTTP `/api/workflows` listing, and the
//! MCP `list_workflows` tool — MUST read from here. Earlier these lived in
//! three places that disagreed on both the step names and their spelling
//! (`todo`/`in-progress` vs `backlog`/`in_progress`), and the orchestrator
//! ignored the card's workflow entirely, so a non-default workflow could never
//! advance correctly.

use serde::Serialize;

/// One named workflow: an id, a human label, and its ordered steps. The first
/// step is always the intake/`backlog` state and the last is always `done`.
#[derive(Debug, Clone, Serialize)]
pub struct Workflow {
    pub id: &'static str,
    pub name: &'static str,
    pub steps: &'static [&'static str],
}

/// Id used whenever a card/project names no workflow, or names one we don't
/// recognize.
pub const DEFAULT_WORKFLOW_ID: &str = "default";

/// All built-in workflows. Every `steps` list starts with `backlog` and ends
/// with `done`; the orchestrator's dispatch auto-advance and `find_next_step`
/// both rely on that shape.
pub const WORKFLOWS: &[Workflow] = &[
    Workflow {
        id: "default",
        name: "Default",
        steps: &["backlog", "in_progress", "review", "done"],
    },
    Workflow {
        id: "simple",
        name: "Simple",
        steps: &["backlog", "in_progress", "done"],
    },
    Workflow {
        id: "research",
        name: "Research",
        steps: &["backlog", "research", "summarize", "done"],
    },
    Workflow {
        id: "full",
        name: "Full Pipeline",
        steps: &["backlog", "design", "implement", "test", "review", "done"],
    },
];

/// Look up a workflow by exact id.
pub fn workflow_by_id(id: &str) -> Option<&'static Workflow> {
    WORKFLOWS.iter().find(|w| w.id == id)
}

/// The default workflow definition. Infallible — `default` is always present in
/// [`WORKFLOWS`].
pub fn default_workflow() -> &'static Workflow {
    workflow_by_id(DEFAULT_WORKFLOW_ID).expect("default workflow must exist")
}

/// Resolve a workflow id to its ordered steps, falling back to the default
/// workflow when the id is `None` or unrecognized.
///
/// Callers pass `card.workflow.as_deref().or(project.default_workflow.as_deref())`
/// so a card's own choice wins, then the project default, then `default`.
pub fn steps_for(id: Option<&str>) -> Vec<String> {
    let wf = id.and_then(workflow_by_id).unwrap_or_else(default_workflow);
    wf.steps.iter().map(|s| s.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_workflow_starts_at_backlog_and_ends_at_done() {
        for wf in WORKFLOWS {
            assert_eq!(wf.steps.first(), Some(&"backlog"), "{} start", wf.id);
            assert_eq!(wf.steps.last(), Some(&"done"), "{} end", wf.id);
            assert!(wf.steps.len() >= 2, "{} needs >= 2 steps", wf.id);
        }
    }

    #[test]
    fn unknown_and_missing_ids_fall_back_to_default() {
        let default = default_workflow().steps;
        assert_eq!(steps_for(None), default);
        assert_eq!(steps_for(Some("does-not-exist")), default);
    }

    #[test]
    fn known_ids_resolve_to_their_own_steps() {
        assert_eq!(
            steps_for(Some("research")),
            vec!["backlog", "research", "summarize", "done"]
        );
        assert_eq!(
            steps_for(Some("simple")),
            vec!["backlog", "in_progress", "done"]
        );
    }
}
