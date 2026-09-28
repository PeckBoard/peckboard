//! Canonical workflow definitions — the single source of truth for the ordered
//! steps a card moves through and the per-step instructions that get appended
//! to the worker prompt.
//!
//! Both `cards.workflow` and `projects.workflow` are NOT NULL columns. A card
//! stores its workflow id at create time — copied from the owning project when
//! the create request doesn't name one explicitly — so step resolution reads
//! `card.workflow` directly without falling back to the project. An unknown id
//! still resolves to [`DEFAULT_WORKFLOW_ID`] rather than an empty list, so
//! `complete_step` can always find the next step (an empty list
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

const FAST_DEVELOP_INSTRUCTIONS: &str = "Implement the change described in the card.

Read any relevant existing code first, then make the change.

- After you finish the change, run the project's tests, linter, and any \
  other checks the repo configures (formatter, type checker, build, etc). \
  If nothing is set up, at minimum run a type check. Fix anything that \
  surfaces — don't leave the card done with red checks behind it.
- If the card is underspecified or you hit a blocker you can't resolve, \
  write up what you found and call `finish_card` so a human can decide \
  what to do.
- If the card CANNOT or SHOULD NOT be completed (infeasible, wrong scope, \
  hard blocker, superseded), call `wont_do_card` with a short reason.
- If the card is larger than one focused pass should be, use \
  `create_card` to split out follow-ups and keep this card's scope \
  narrow.

When done, call `finish_card`.";

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

/// Name of the shared review step every workflow runs right before `done`:
/// a fresh worker session — optionally on the project's `review_model` —
/// independently verifies the card. Projects can turn it off
/// (`projects.review_enabled`), see [`steps_for_card`].
pub const REVIEW_STEP: &str = "review";

/// Instructions for the shared [`REVIEW_STEP`]. The worker prompt adds the
/// previous worker's session id and handoff above these (see
/// `pipeline::build_worker_prompt`).
pub const REVIEW_INSTRUCTIONS: &str =
    "Independently review the work a PREVIOUS worker (a different session) \
did on this card. You are a second pair of eyes; the author's own account \
of its work is not evidence.

### Ground Rules

- DO NOT trust claims that the work is complete — not in the handoff, \
  not in the previous worker's transcript, not in commit messages. Every \
  claim is a hypothesis until you have checked it yourself.
- Independently verify EVERY requirement in the card (title, description, \
  acceptance criteria) against the actual code, files, reports, and \
  tests. Run the tests / build / linter yourself; don't rely on reported \
  results.

### Find the Work

- Read the previous worker's handoff and its full transcript via \
  `read_worker_session` (session id in the Review Handoff section) to \
  learn what it claims to have done and where.
- **If the working directory is a git repo** and the handoff names a \
  branch, check out that branch and review its commits / diff against \
  the base branch. Otherwise review the working tree and recent commits \
  (`git log`, `git diff`). Do NOT push unless instructed to.
- If the working directory is NOT a git repo, review the files in place.

### Review

- **Completeness** — tick off each requirement in the card one by one.
- **Correctness** — does it actually do what the card asks? Trace the \
  logic; don't just read the tests. Look for off-by-one, wrong branch, \
  missed edge cases.
- **Security** — input validation at boundaries, no secrets in code, no \
  obvious injection/XSS/SSRF.
- **Tests** — are the new behaviours and edge cases covered, and do the \
  checks pass?
- **Style & scope** — no unrelated refactors, no dead code, names make \
  sense.

### Act on What You Find

- Small defects in the delivered work (a bug, a missing test, a failing \
  check) → fix them directly in this session and re-run the checks. On a \
  branch, commit the fixes on that same branch.
- EVERY requirement that is missing or only partly done → call \
  `create_card` in this same project describing the gap: what is missing, \
  where, and how to verify it. Reference the origin card (title + id) in \
  the description and pass the origin card's workflow as `workflow`. Do \
  NOT silently finish the missing work yourself and do NOT drop it.
- Don't call `complete_step` — this is the last step before done.

### Finish

Call `finish_card` with a summary listing each requirement and how you \
verified it, what you fixed, and the gap cards you created (title + id) \
or \"no gaps\". If the work is fundamentally wrong or the card should \
not be done, call `wont_do_card` with the reason instead.";

/// All built-in workflows. Every `steps` list starts with `backlog`, ends
/// with `done`, and runs the shared [`REVIEW_STEP`] right before `done`;
/// the orchestrator's dispatch auto-advance and `find_next_step` both rely
/// on that shape.
pub const WORKFLOWS: &[Workflow] = &[
    Workflow {
        id: "task",
        name: "Task",
        description: "Runs a single in-progress step, then a fresh session reviews the \
                      result. Best for everyday one-shot jobs you would hand a worker.",
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
                step: REVIEW_STEP,
                instructions: REVIEW_INSTRUCTIONS,
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
                step: REVIEW_STEP,
                instructions: REVIEW_INSTRUCTIONS,
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
                step: REVIEW_STEP,
                instructions: REVIEW_INSTRUCTIONS,
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
        description: "Normal software development: implement, run the checks, then an \
                      independent review. Lower cost than Deep Develop Software.",
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
                step: REVIEW_STEP,
                instructions: REVIEW_INSTRUCTIONS,
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
        description: "For big or riskier tasks: the work lands on its own git branch with \
                      an explicit hand-off, then an independent reviewer checks that \
                      branch. Higher cost.",
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
                step: REVIEW_STEP,
                instructions: REVIEW_INSTRUCTIONS,
            },
            WorkflowStep {
                step: "done",
                instructions: "",
            },
        ],
    },
];

/// Owned, serializable form of a workflow step. Built-ins convert into
/// this via [`WorkflowStep`]'s `From` impl below; custom workflows are
/// stored this way directly (see `db::crud::custom_workflows`).
#[derive(Debug, Clone, Serialize)]
pub struct WorkflowStepDef {
    pub step: String,
    pub instructions: String,
}

impl From<&WorkflowStep> for WorkflowStepDef {
    fn from(s: &WorkflowStep) -> Self {
        WorkflowStepDef {
            step: s.step.to_string(),
            instructions: s.instructions.to_string(),
        }
    }
}

/// Owned, serializable form of a workflow — the shape every caller (HTTP
/// routes, MCP handlers, the orchestrator) actually works with. Built-ins
/// convert from the `&'static Workflow` registry below; user-defined
/// workflows are loaded from the `custom_workflows` DB table into the
/// in-memory registry at startup and after every mutation.
///
/// `&'static` return types don't work once workflows can be created at
/// runtime, so every public lookup in this module returns an owned
/// `WorkflowDef` rather than a reference into `WORKFLOWS`.
#[derive(Debug, Clone, Serialize)]
pub struct WorkflowDef {
    pub id: String,
    pub name: String,
    pub description: String,
    pub priority: u32,
    /// "builtin" or "custom" — lets the picker and management UI tag
    /// entries and lets routes reject writes to built-ins.
    pub source: &'static str,
    pub steps: Vec<WorkflowStepDef>,
}

impl From<&'static Workflow> for WorkflowDef {
    fn from(w: &'static Workflow) -> Self {
        WorkflowDef {
            id: w.id.to_string(),
            name: w.name.to_string(),
            description: w.description.to_string(),
            priority: w.priority,
            source: "builtin",
            steps: w.steps.iter().map(WorkflowStepDef::from).collect(),
        }
    }
}

/// In-memory registry of user-defined workflows, primed from the DB at
/// startup and refreshed after every create/update/delete. Built-ins never
/// go in here — they stay in the `WORKFLOWS` const above and are always
/// read-only.
static CUSTOM_WORKFLOWS: std::sync::RwLock<Vec<WorkflowDef>> = std::sync::RwLock::new(Vec::new());

/// Replace the full set of custom workflows held in memory. Called at
/// startup (after loading from the DB) and after any CRUD mutation so
/// every reader — the orchestrator, the worker prompt builder, the HTTP
/// listing — sees the change immediately without a DB round-trip. Each
/// definition gets the shared review step (see [`with_review_step`]).
pub fn set_custom_workflows(defs: Vec<WorkflowDef>) {
    let defs = defs.into_iter().map(with_review_step).collect();
    let mut guard = CUSTOM_WORKFLOWS
        .write()
        .expect("workflow registry poisoned");
    *guard = defs;
}

/// Give a custom workflow the shared [`REVIEW_STEP`] right before `done`,
/// unless it already declares its own `review` step. The injected step is
/// never persisted: [`is_injected_review_step`] lets the save path drop it
/// again when the editor round-trips it unchanged, so stored workflows keep
/// tracking the current shared instructions.
pub fn with_review_step(mut def: WorkflowDef) -> WorkflowDef {
    let has_review = def.steps.iter().any(|s| s.step == REVIEW_STEP);
    let ends_at_done = def.steps.last().is_some_and(|s| s.step == "done");
    if !has_review && ends_at_done {
        let at = def.steps.len() - 1;
        def.steps.insert(
            at,
            WorkflowStepDef {
                step: REVIEW_STEP.to_string(),
                instructions: REVIEW_INSTRUCTIONS.to_string(),
            },
        );
    }
    def
}

/// True for a `review` step that is exactly the injected shared step (see
/// [`with_review_step`]) — i.e. the user didn't customise it.
pub fn is_injected_review_step(step: &WorkflowStepDef) -> bool {
    step.step == REVIEW_STEP && step.instructions.trim() == REVIEW_INSTRUCTIONS.trim()
}

/// Every workflow — built-ins first (in their fixed order), then custom
/// workflows. Callers that need a stable display order (the picker) sort
/// by `priority`/`name` themselves; this just returns the full set.
pub fn all_workflows() -> Vec<WorkflowDef> {
    let mut all: Vec<WorkflowDef> = WORKFLOWS.iter().map(WorkflowDef::from).collect();
    all.extend(
        CUSTOM_WORKFLOWS
            .read()
            .expect("workflow registry poisoned")
            .iter()
            .cloned(),
    );
    all
}

/// Look up a built-in workflow by exact id. Built-ins only — use
/// [`workflow_by_id`] to resolve across built-in + custom.
fn builtin_by_id(id: &str) -> Option<&'static Workflow> {
    WORKFLOWS.iter().find(|w| w.id == id)
}

/// Look up a workflow by exact id across built-ins and custom workflows.
pub fn workflow_by_id(id: &str) -> Option<WorkflowDef> {
    if let Some(w) = builtin_by_id(id) {
        return Some(WorkflowDef::from(w));
    }
    CUSTOM_WORKFLOWS
        .read()
        .expect("workflow registry poisoned")
        .iter()
        .find(|w| w.id == id)
        .cloned()
}

/// The default workflow definition. Infallible — the default id is always
/// present in [`WORKFLOWS`].
pub fn default_workflow() -> WorkflowDef {
    WorkflowDef::from(builtin_by_id(DEFAULT_WORKFLOW_ID).expect("default workflow must exist"))
}

/// Resolve a workflow id to its ordered step names, falling back to the
/// default workflow when the id is `None` or unrecognized.
///
/// Callers pass `Some(&card.workflow)` — the card's workflow is NOT NULL,
/// so resolution doesn't need to consult the project. The `Option` wrapper
/// stays for the rare unknown-id case and for tests that need to exercise
/// the fallback path.
pub fn steps_for(id: Option<&str>) -> Vec<String> {
    let wf = id.and_then(workflow_by_id).unwrap_or_else(default_workflow);
    wf.steps.into_iter().map(|s| s.step).collect()
}

/// Step order for one card: its workflow's steps, minus the shared
/// [`REVIEW_STEP`] when the owning project turned review off. A card already
/// sitting on `review` keeps the step so it stays resolvable and can finish.
/// Everything that advances a card (`complete_step`, `finish_card`, the
/// orchestrator's completion handler) must use this rather than
/// [`steps_for`], or a review-disabled project would still stop on review.
pub fn steps_for_card(workflow_id: &str, current_step: &str, review_enabled: bool) -> Vec<String> {
    let mut steps = steps_for(Some(workflow_id));
    if !review_enabled && current_step != REVIEW_STEP {
        steps.retain(|s| s != REVIEW_STEP);
    }
    steps
}

/// Where `finish_card` lands from `current_step`: the [`REVIEW_STEP`] while
/// the card still has an unrun review ahead of it in `steps` (as returned by
/// [`steps_for_card`]), otherwise `done`. An unknown current step never
/// invents a review detour.
pub fn finish_target(current_step: &str, steps: &[String]) -> String {
    let current = steps.iter().position(|s| s == current_step);
    let review = steps.iter().position(|s| s == REVIEW_STEP);
    match (current, review) {
        (Some(c), Some(r)) if c < r => REVIEW_STEP.to_string(),
        _ => "done".to_string(),
    }
}

/// Look up the per-step instructions for a workflow/step combination. Returns
/// `None` when the workflow doesn't define the step or the step's
/// instructions are empty.
pub fn step_instructions(workflow_id: Option<&str>, step: &str) -> Option<String> {
    let wf = workflow_id
        .and_then(workflow_by_id)
        .unwrap_or_else(default_workflow);
    wf.steps
        .into_iter()
        .find(|s| s.step == step)
        .map(|s| s.instructions)
        .filter(|i| !i.is_empty())
}
/// Max steps a custom workflow may declare. Generous enough for any real
/// use case; guards against pathological input.
const MAX_CUSTOM_WORKFLOW_STEPS: usize = 12;
const MAX_CUSTOM_WORKFLOW_NAME_LEN: usize = 200;

/// Validate a custom workflow's name: non-empty, length-bounded, and not a
/// case-insensitive collision with any existing workflow (built-in or
/// custom) other than `self_id` (the workflow being updated, if any).
pub fn validate_workflow_name(name: &str, self_id: Option<&str>) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("name must not be empty".to_string());
    }
    if name.chars().count() > MAX_CUSTOM_WORKFLOW_NAME_LEN {
        return Err(format!(
            "name must be at most {MAX_CUSTOM_WORKFLOW_NAME_LEN} characters"
        ));
    }
    let lower = name.trim().to_lowercase();
    let collides = all_workflows()
        .into_iter()
        .any(|w| Some(w.id.as_str()) != self_id && w.name.trim().to_lowercase() == lower);
    if collides {
        return Err(format!("a workflow named '{name}' already exists"));
    }
    Ok(())
}

/// Validate a custom workflow's step list against the same shape the
/// orchestrator requires of every workflow (see the `every_workflow_*`
/// test below): starts at `backlog`, ends at `done`, unique step names,
/// and every non-terminal step carries non-empty instructions (a step
/// with no instructions never gets a worker prompt to run, and an empty
/// per-project override on it is rejected by
/// `PUT /api/projects/:id/workflow-instructions`).
pub fn validate_workflow_steps(steps: &[WorkflowStepDef]) -> Result<(), String> {
    if steps.len() < 3 {
        return Err("a workflow needs at least 3 steps (backlog, one working step, done)".into());
    }
    if steps.len() > MAX_CUSTOM_WORKFLOW_STEPS {
        return Err(format!(
            "a workflow may have at most {MAX_CUSTOM_WORKFLOW_STEPS} steps"
        ));
    }
    if steps.first().map(|s| s.step.as_str()) != Some("backlog") {
        return Err("the first step must be 'backlog'".into());
    }
    if steps.last().map(|s| s.step.as_str()) != Some("done") {
        return Err("the last step must be 'done'".into());
    }
    let mut seen = std::collections::HashSet::new();
    for s in steps {
        if !STEP_NAME_RE.is_match(&s.step) {
            return Err(format!(
                "step '{}' is invalid: step names must be lowercase, start with a letter, and \
                 contain only letters, digits, and underscores",
                s.step
            ));
        }
        if !seen.insert(s.step.clone()) {
            return Err(format!("step '{}' is defined more than once", s.step));
        }
    }
    for s in &steps[1..steps.len() - 1] {
        if matches!(s.step.as_str(), "backlog" | "todo" | "done" | "wont_do") {
            return Err(format!(
                "step '{}' is reserved and may not be used as a working step",
                s.step
            ));
        }
    }
    for (i, s) in steps.iter().enumerate() {
        let is_terminal = i == 0 || i == steps.len() - 1;
        if is_terminal && !s.instructions.trim().is_empty() {
            return Err(format!(
                "step '{}' is terminal and must not have instructions",
                s.step
            ));
        }
        if !is_terminal && s.instructions.trim().is_empty() {
            return Err(format!(
                "step '{}' must have non-empty instructions — a step with no \
                 instructions never runs a worker",
                s.step
            ));
        }
    }
    Ok(())
}

static STEP_NAME_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"^[a-z][a-z0-9_]*$").unwrap());

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
            vec!["backlog", "in_progress", "review", "done"]
        );
    }

    #[test]
    fn every_workflow_reviews_right_before_done() {
        for wf in all_workflows() {
            let n = wf.steps.len();
            assert_eq!(wf.steps[n - 2].step, REVIEW_STEP, "{} review", wf.id);
            assert_eq!(wf.steps[n - 1].step, "done", "{} end", wf.id);
        }
    }

    #[test]
    fn step_instructions_are_returned_for_known_combos() {
        // Task's in_progress step has instructions.
        let inst = step_instructions(Some("task"), "in_progress").unwrap();
        assert!(inst.contains("Do the task"));
        // Every workflow shares one review step, distinct from execution.
        let review = step_instructions(Some("deep-develop-software"), "review").unwrap();
        assert!(review.contains("DO NOT trust"));
        assert_eq!(
            step_instructions(Some("task"), "review").as_deref(),
            Some(review.as_str())
        );
        let exec = step_instructions(Some("deep-develop-software"), "in_progress").unwrap();
        assert!(exec.contains("Implement the work"));
        assert_ne!(review, exec);
    }

    #[test]
    fn step_instructions_returns_none_for_empty_or_missing_steps() {
        // Terminal step has no instructions.
        assert!(step_instructions(Some("task"), "done").is_none());
        // Unknown step on a known workflow.
        assert!(step_instructions(Some("task"), "summarize").is_none());
        // Unknown workflow falls back to default.
        assert!(step_instructions(Some("does-not-exist"), "summarize").is_none());
    }

    fn valid_custom_steps() -> Vec<WorkflowStepDef> {
        vec![
            WorkflowStepDef {
                step: "backlog".to_string(),
                instructions: String::new(),
            },
            WorkflowStepDef {
                step: "in_progress".to_string(),
                instructions: "Do the thing.".to_string(),
            },
            WorkflowStepDef {
                step: "done".to_string(),
                instructions: String::new(),
            },
        ]
    }

    #[test]
    fn validate_workflow_steps_accepts_a_well_formed_workflow() {
        assert!(validate_workflow_steps(&valid_custom_steps()).is_ok());
    }

    #[test]
    fn validate_workflow_steps_rejects_wrong_start_or_end() {
        let mut steps = valid_custom_steps();
        steps[0].step = "todo".to_string();
        assert!(validate_workflow_steps(&steps).is_err());

        let mut steps = valid_custom_steps();
        steps[2].step = "finished".to_string();
        assert!(validate_workflow_steps(&steps).is_err());
    }

    #[test]
    fn validate_workflow_steps_rejects_empty_middle_instructions() {
        let mut steps = valid_custom_steps();
        steps[1].instructions = String::new();
        assert!(validate_workflow_steps(&steps).is_err());
    }

    #[test]
    fn validate_workflow_steps_rejects_instructions_on_terminal_steps() {
        let mut steps = valid_custom_steps();
        steps[0].instructions = "should not be here".to_string();
        assert!(validate_workflow_steps(&steps).is_err());
    }

    #[test]
    fn validate_workflow_steps_rejects_duplicate_or_malformed_step_names() {
        let mut steps = valid_custom_steps();
        steps[1].step = "backlog".to_string();
        assert!(validate_workflow_steps(&steps).is_err());

        let mut steps = valid_custom_steps();
        steps[1].step = "In Progress".to_string();
        assert!(validate_workflow_steps(&steps).is_err());
    }

    #[test]
    fn validate_workflow_steps_rejects_reserved_names_in_middle_positions() {
        // The orchestrator gives these names special meaning (intake rewind /
        // terminal filtering); re-declaring one as a working step would loop or
        // wedge the card.
        for reserved in ["backlog", "todo", "done", "wont_do"] {
            let mut steps = valid_custom_steps();
            steps[1].step = reserved.to_string();
            let err = validate_workflow_steps(&steps)
                .expect_err("reserved step name must be rejected in a middle position");
            assert!(err.contains(reserved), "unexpected error: {err}");
        }
    }

    #[test]
    fn validate_workflow_name_rejects_builtin_collision_case_insensitively() {
        assert!(validate_workflow_name("task", None).is_err());
        assert!(validate_workflow_name("TASK", None).is_err());
        assert!(validate_workflow_name("My New Workflow", None).is_ok());
    }

    #[test]
    fn validate_workflow_name_rejects_empty() {
        assert!(validate_workflow_name("   ", None).is_err());
    }

    // Registry tests share the process-wide `CUSTOM_WORKFLOWS` static, so
    // they run as one test to avoid interleaving with other `#[test]`
    // threads that might call `set_custom_workflows`.
    #[test]
    fn registry_merges_builtins_and_custom_workflows() {
        let custom = WorkflowDef {
            id: "my-custom-id".to_string(),
            name: "My Custom Flow".to_string(),
            description: "desc".to_string(),
            priority: 1000,
            source: "custom",
            steps: valid_custom_steps(),
        };
        set_custom_workflows(vec![custom]);

        let all = all_workflows();
        assert!(all.iter().any(|w| w.id == "task" && w.source == "builtin"));
        assert!(
            all.iter()
                .any(|w| w.id == "my-custom-id" && w.source == "custom")
        );

        let resolved = workflow_by_id("my-custom-id").expect("custom workflow must resolve");
        assert_eq!(resolved.name, "My Custom Flow");
        // The shared review step is injected before `done`.
        assert_eq!(
            steps_for(Some("my-custom-id")),
            vec!["backlog", "in_progress", "review", "done"]
        );
        assert_eq!(
            step_instructions(Some("my-custom-id"), "in_progress").as_deref(),
            Some("Do the thing.")
        );
        assert_eq!(
            step_instructions(Some("my-custom-id"), "review").as_deref(),
            Some(REVIEW_INSTRUCTIONS)
        );

        // A custom workflow that declares its own `review` keeps it (no
        // second review step, its own instructions).
        let mut own_review = valid_custom_steps();
        own_review.insert(
            2,
            WorkflowStepDef {
                step: "review".to_string(),
                instructions: "Custom review.".to_string(),
            },
        );
        set_custom_workflows(vec![WorkflowDef {
            id: "my-custom-review".to_string(),
            name: "My Custom Review".to_string(),
            description: String::new(),
            priority: 1000,
            source: "custom",
            steps: own_review,
        }]);
        assert_eq!(
            steps_for(Some("my-custom-review")),
            vec!["backlog", "in_progress", "review", "done"]
        );
        assert_eq!(
            step_instructions(Some("my-custom-review"), "review").as_deref(),
            Some("Custom review.")
        );

        // Clean up so this test doesn't leak into any test added later in
        // this module.
        set_custom_workflows(Vec::new());
    }

    #[test]
    fn review_can_be_skipped_per_project_but_not_mid_review() {
        assert_eq!(
            steps_for_card("task", "in_progress", false),
            vec!["backlog", "in_progress", "done"]
        );
        // A card already on `review` keeps it resolvable so it can finish.
        assert_eq!(
            steps_for_card("task", "review", false),
            vec!["backlog", "in_progress", "review", "done"]
        );
        assert_eq!(
            steps_for_card("task", "in_progress", true),
            vec!["backlog", "in_progress", "review", "done"]
        );
    }

    #[test]
    fn finish_lands_on_review_until_the_card_was_reviewed() {
        let with_review = steps_for_card("task", "in_progress", true);
        assert_eq!(finish_target("in_progress", &with_review), "review");
        assert_eq!(finish_target("review", &with_review), "done");
        let without = steps_for_card("task", "in_progress", false);
        assert_eq!(finish_target("in_progress", &without), "done");
        // Unknown step: never invent a review detour.
        assert_eq!(finish_target("mystery", &with_review), "done");
    }
}
