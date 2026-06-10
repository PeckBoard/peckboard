//! Canonical workflow definitions — the single source of truth for the ordered
//! steps a card moves through and the per-step instructions that get appended
//! to the worker prompt.
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
//!
//! Each workflow also carries a human-facing `description` (shown in the
//! workflow picker) and optional per-step `instructions` that the worker
//! prompt builder appends as the step's marching orders.

use serde::Serialize;

/// One step in a workflow: the canonical board step name plus the
/// instructions a worker should follow during this step. Empty
/// instructions leave the step using only the generic per-step prompt
/// (no extra guidance).
#[derive(Debug, Clone, Serialize)]
pub struct WorkflowStep {
    pub step: &'static str,
    pub instructions: &'static str,
}

/// One named workflow: an id, a human label, a description shown in the
/// picker, a sort priority (lower = earlier in the list), and its ordered
/// steps. The first step is always the intake/`backlog` state and the last
/// is always `done`.
#[derive(Debug, Clone, Serialize)]
pub struct Workflow {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub priority: u32,
    pub steps: &'static [WorkflowStep],
}

/// Id used whenever a card/project names no workflow, or names one we don't
/// recognize.
pub const DEFAULT_WORKFLOW_ID: &str = "task";

const TASK_INSTRUCTIONS: &str = "Do the task described in the card.

- Read whatever context you need to understand what's being asked.
- Complete the work end-to-end. If the card asks for code, make the code \
  changes; if it asks for a report, write the report; if it asks for both, \
  do both.
- If you need to leave any breadcrumbs for a human (files produced, where \
  things landed), include them in the handoff_context you pass to \
  `finish_card`.
- If the card cannot or should not be completed, call `wont_do_card` with a \
  short reason.
- When you're done, call `finish_card`.";

const RESEARCH_INSTRUCTIONS: &str =
    "Investigate the question described in the card and write up what you found. \
Do NOT file follow-up cards and do NOT make code changes — this card is \
investigation only.

### Investigate

- Dig into code, docs, tickets, external sources — whatever it takes to \
  answer the question fully.
- Read widely before drawing conclusions. Cite specific file paths, line \
  numbers, URLs, or card IDs where you find evidence.
- Capture surprising findings, dead ends, and open questions as you go — \
  those are often as valuable as the main answer.
- Don't stop at the first plausible answer. Check edge cases, look for \
  contradicting evidence, and note anything that would invalidate your \
  conclusion.

### Write the report

- Use the `write_report` MCP tool to save a Markdown report. Give it a \
  clear title. Include sections for: the question, the answer, the \
  evidence, and open questions / caveats.
- If the question is trivial or the answer is a one-liner, it's fine to \
  skip the report and put the answer in the handoff_context you pass to \
  `finish_card`.

### Do NOT file follow-up cards

- This card is scoped to research output only. If the investigation \
  surfaces work that should be done, mention it in the report's \
  \"open questions\" section — a human will decide whether to file cards \
  for it.
- Do NOT call `create_card`. Do NOT make code changes.

### Finish

- If the card cannot or should not be done, call `wont_do_card` with a \
  short reason.
- Otherwise call `finish_card` with a short handoff_context like \
  \"report in folder <name>\" or, if no report was written, a one-line \
  summary of the answer.";

const BREAKDOWN_INSTRUCTIONS: &str =
    "Investigate the card, then break the work down into follow-up cards \
other workers can pick up.

### Investigate

- Dig into code, docs, tickets, external sources — whatever it takes to \
  understand the scope fully.
- Read widely before drawing conclusions. Cite specific file paths, line \
  numbers, URLs, or card IDs where you find evidence.
- Capture surprising findings, dead ends, and open questions as you go — \
  those are often as valuable as the main answer.
- Don't stop at the first plausible plan. Check edge cases, look for \
  contradicting evidence, and note anything that would invalidate your \
  breakdown.

### Write a short report

- Use the `write_report` MCP tool to save a Markdown report that captures \
  the context behind the breakdown. Include sections for: the problem, \
  the plan, the evidence, and open questions / caveats.

### File follow-up cards (the main deliverable)

- Call `create_card` for every unit of work the breakdown surfaced. Keep \
  titles action-oriented (\"Investigate memory spike in X\", \"Migrate Y \
  to new API\"). Reference the report folder in each card's description \
  so whoever picks it up can find the context.

### Finish

- If the card is infeasible or shouldn't be done, call `wont_do_card` \
  with a short reason.
- Otherwise call `finish_card` with a short handoff_context like \
  \"report in folder <name>; filed N follow-up cards\".";

const FAST_DEVELOP_INSTRUCTIONS: &str =
    "Implement and self-review the change described in the card.

### Implement

Read any relevant existing code first, then make the change.

- **If the working directory is a git repo**, unless the card gives \
  specific git instructions, do your work on its own branch. Pick a \
  short, descriptive branch name; if a branch already exists for this \
  card, reuse it. Commit your changes on that branch. Do NOT push \
  unless instructed to.
- If the working directory is NOT a git repo, just edit the files \
  directly in place. No branch needed.
- Keep the change tight to what the card actually asks for — don't \
  refactor unrelated code, add speculative abstractions, or expand \
  scope.
- Run the project's tests and linter. If nothing is set up, at minimum \
  run a type check.
- If the card is underspecified or you hit a blocker you can't resolve, \
  write up what you found and call `finish_card` so a human can decide \
  what to do.
- If the card CANNOT or SHOULD NOT be completed (infeasible, wrong scope, \
  hard blocker, superseded), call `wont_do_card` with a short reason.
- If the card is larger than one focused pass should be, use \
  `create_card` to split out follow-ups and keep this card's scope \
  narrow.

### Self-review

Review your own change as if it were a pull request:

- **Correctness** — does the code actually solve the card? Trace the \
  logic; don't just read the tests. Look for off-by-one, wrong branch, \
  missed edge cases.
- **Security** — input validation at boundaries, no secrets in code, no \
  obvious injection/XSS/SSRF.
- **Tests** — did you add tests for the new behavior and edge cases? If \
  tests are missing, add them now or file a follow-up with `create_card`.
- **Style & scope** — no unrelated refactors, no dead code, names make \
  sense, comments only where the \"why\" is non-obvious.

Fix any issues you find directly on the same branch, then re-run tests \
+ lint.

When done, call `finish_card`. If you created a branch, include it in \
handoff_context (e.g. `\"branch feat/queue\"`).";

const DEEP_DEVELOP_EXECUTION_INSTRUCTIONS: &str =
    "Implement the work described in the card. Read any relevant existing \
code first, then make the change.

- **If the working directory is a git repo**, unless the card gives \
  specific git instructions, do your work on its own branch. Pick a \
  short, descriptive branch name; if a branch already exists for this \
  card, reuse it. Commit your changes on that branch. Do NOT push \
  unless instructed to. When you call `complete_step`, the \
  handoff_context MUST include the branch name (e.g. \
  `\"branch feat/queue\"`).
- If the working directory is NOT a git repo, just edit the files \
  directly in place. No branch needed.
- Keep the change tight to what the card actually asks for — don't \
  refactor unrelated code, add speculative abstractions, or expand \
  scope.
- Run the project's tests and linter before you finish. If nothing is \
  set up, at minimum run a type check.
- If the card is underspecified or you hit a blocker you can't resolve, \
  write up what you found and call `finish_card` so a human can decide \
  what to do.
- If the card CANNOT or SHOULD NOT be completed (infeasible, wrong \
  scope, hard blocker, superseded), call `wont_do_card` with a short \
  reason.
- If the card is larger than one focused pass should be, use \
  `create_card` to split out follow-ups and keep this card's scope \
  narrow.

When the implementation is complete and ready for review, call \
`complete_step` to hand off to the review step.";

const DEEP_DEVELOP_REVIEW_INSTRUCTIONS: &str =
    "Review the code change produced by the previous worker. Treat the \
previous worker's output as a pull request.

If the working directory is NOT a git repo, do the review against the \
files in place.

**If the working directory is a git repo**, the previous worker's \
handoff_context should include the branch name (look for `branch \
<name>` in the handoff context near the top of this prompt). If the \
handoff context is missing or the branch doesn't exist, call \
`finish_card` with a short reason rather than reviewing a stale copy \
of main.

### Review

- `cd` to the working directory and check out the branch named in the \
  handoff. Do the review against that branch's commits.
- **Correctness** — does the code actually solve the card? Trace the \
  logic; don't just read the tests. Look for off-by-one, wrong branch, \
  missed edge cases.
- **Security** — input validation at boundaries, no secrets in code, no \
  obvious injection/XSS/SSRF.
- **Tests** — did the author add tests for the new behavior and edge \
  cases? If tests are missing, either add them or file a follow-up.
- **Style & scope** — no unrelated refactors, no dead code, names make \
  sense, comments only where the \"why\" is non-obvious.

If you find issues, fix them directly in this session (you have the \
same tools as the previous worker). Don't punt to \"future work\" \
unless it's genuinely out of scope — in that case file a new card with \
`create_card`. If you make fixes, commit them on the same branch. Do \
NOT push unless instructed to.

When done, call `finish_card`. The handoff_context should be short \
and final, e.g. `\"review complete; branch feat/queue ready\"`.";

/// All built-in workflows. Every `steps` list starts with `backlog` and ends
/// with `done`; the orchestrator's dispatch auto-advance and `find_next_step`
/// both rely on that shape.
pub const WORKFLOWS: &[Workflow] = &[
    Workflow {
        id: "task",
        name: "Task",
        description: "Runs a single in-progress step with no review. Best for everyday \
                      one-shot jobs you would hand a worker without needing a separate \
                      review step.",
        priority: 100,
        steps: &[
            WorkflowStep {
                step: "backlog",
                instructions: "",
            },
            WorkflowStep {
                step: "in_progress",
                instructions: TASK_INSTRUCTIONS,
            },
            WorkflowStep {
                step: "done",
                instructions: "",
            },
        ],
    },
    Workflow {
        id: "research",
        name: "Research",
        description: "Investigate a question and write up what you found. No follow-up \
                      cards, no code changes. Use when you just want an answer or a \
                      report — not a to-do list.",
        priority: 200,
        steps: &[
            WorkflowStep {
                step: "backlog",
                instructions: "",
            },
            WorkflowStep {
                step: "in_progress",
                instructions: RESEARCH_INSTRUCTIONS,
            },
            WorkflowStep {
                step: "done",
                instructions: "",
            },
        ],
    },
    Workflow {
        id: "breakdown",
        name: "Breakdown",
        description: "You have an idea for a task but it needs research first. No real \
                      work is done — the card gets broken down into smaller cards.",
        priority: 300,
        steps: &[
            WorkflowStep {
                step: "backlog",
                instructions: "",
            },
            WorkflowStep {
                step: "in_progress",
                instructions: BREAKDOWN_INSTRUCTIONS,
            },
            WorkflowStep {
                step: "done",
                instructions: "",
            },
        ],
    },
    Workflow {
        id: "fast-develop-software",
        name: "Fast Develop Software",
        description: "Normal software development with built-in self-review. Lower cost \
                      than Deep Develop Software.",
        priority: 400,
        steps: &[
            WorkflowStep {
                step: "backlog",
                instructions: "",
            },
            WorkflowStep {
                step: "in_progress",
                instructions: FAST_DEVELOP_INSTRUCTIONS,
            },
            WorkflowStep {
                step: "done",
                instructions: "",
            },
        ],
    },
    Workflow {
        id: "deep-develop-software",
        name: "Deep Develop Software",
        description: "For big or riskier tasks where you want a second worker to review \
                      the changes after the first one implements them. Higher cost.",
        priority: 500,
        steps: &[
            WorkflowStep {
                step: "backlog",
                instructions: "",
            },
            WorkflowStep {
                step: "in_progress",
                instructions: DEEP_DEVELOP_EXECUTION_INSTRUCTIONS,
            },
            WorkflowStep {
                step: "review",
                instructions: DEEP_DEVELOP_REVIEW_INSTRUCTIONS,
            },
            WorkflowStep {
                step: "done",
                instructions: "",
            },
        ],
    },
];

/// Look up a workflow by exact id.
pub fn workflow_by_id(id: &str) -> Option<&'static Workflow> {
    WORKFLOWS.iter().find(|w| w.id == id)
}

/// The default workflow definition. Infallible — the default id is always
/// present in [`WORKFLOWS`].
pub fn default_workflow() -> &'static Workflow {
    workflow_by_id(DEFAULT_WORKFLOW_ID).expect("default workflow must exist")
}

/// Resolve a workflow id to its ordered step names, falling back to the
/// default workflow when the id is `None` or unrecognized.
///
/// Callers pass `card.workflow.as_deref().or(project.default_workflow.as_deref())`
/// so a card's own choice wins, then the project default, then the default.
pub fn steps_for(id: Option<&str>) -> Vec<String> {
    let wf = id.and_then(workflow_by_id).unwrap_or_else(default_workflow);
    wf.steps.iter().map(|s| s.step.to_string()).collect()
}

/// Look up the per-step instructions for a workflow/step combination. Returns
/// `None` when the workflow doesn't define the step or the step's
/// instructions are empty.
pub fn step_instructions(workflow_id: Option<&str>, step: &str) -> Option<&'static str> {
    let wf = workflow_id
        .and_then(workflow_by_id)
        .unwrap_or_else(default_workflow);
    wf.steps
        .iter()
        .find(|s| s.step == step)
        .map(|s| s.instructions)
        .filter(|i| !i.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_workflow_starts_at_backlog_and_ends_at_done() {
        for wf in WORKFLOWS {
            assert_eq!(
                wf.steps.first().map(|s| s.step),
                Some("backlog"),
                "{} start",
                wf.id
            );
            assert_eq!(
                wf.steps.last().map(|s| s.step),
                Some("done"),
                "{} end",
                wf.id
            );
            assert!(wf.steps.len() >= 2, "{} needs >= 2 steps", wf.id);
        }
    }

    #[test]
    fn unknown_and_missing_ids_fall_back_to_default() {
        let default: Vec<String> = default_workflow()
            .steps
            .iter()
            .map(|s| s.step.to_string())
            .collect();
        assert_eq!(steps_for(None), default);
        assert_eq!(steps_for(Some("does-not-exist")), default);
    }

    #[test]
    fn known_ids_resolve_to_their_own_steps() {
        assert_eq!(
            steps_for(Some("deep-develop-software")),
            vec!["backlog", "in_progress", "review", "done"]
        );
        assert_eq!(
            steps_for(Some("task")),
            vec!["backlog", "in_progress", "done"]
        );
    }

    #[test]
    fn step_instructions_are_returned_for_known_combos() {
        // Task's in_progress step has instructions.
        let inst = step_instructions(Some("task"), "in_progress").unwrap();
        assert!(inst.contains("Do the task"));
        // Deep-develop's review step has its own instructions distinct from
        // the execution step's instructions.
        let review = step_instructions(Some("deep-develop-software"), "review").unwrap();
        assert!(review.contains("Review the code change"));
        let exec = step_instructions(Some("deep-develop-software"), "in_progress").unwrap();
        assert!(exec.contains("Implement the work"));
        assert_ne!(review, exec);
    }

    #[test]
    fn step_instructions_returns_none_for_empty_or_missing_steps() {
        // Terminal step has no instructions.
        assert!(step_instructions(Some("task"), "done").is_none());
        // Unknown step on a known workflow.
        assert!(step_instructions(Some("task"), "review").is_none());
        // Unknown workflow falls back to default, which has no review step.
        assert!(step_instructions(Some("does-not-exist"), "review").is_none());
    }
}
