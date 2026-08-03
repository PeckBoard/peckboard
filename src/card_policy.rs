//! Card-update transition policy, shared by every write path.
//!
//! Two invariants govern edits to an existing card:
//!
//! 1. **Terminal cards** (`done` / `wont_do`) accept step changes only —
//!    that's how a card gets reopened or moved between the terminal
//!    columns. Any other field edit is refused.
//! 2. **Description + workflow freeze once a card leaves `backlog`.** A
//!    worker mid-run has already read the description and is executing
//!    against the card's workflow; re-pointing `workflow` without a
//!    matching `step` leaves the card on a step that doesn't exist in the
//!    new workflow, and the next `complete_step` then finds no successor
//!    and jumps the card straight to `done`, skipping every real step and
//!    prematurely unblocking dependents.
//!
//! The policy used to live inline in the HTTP route's
//! `update_card_atomic` closure, which meant the MCP `update_card`
//! handler — a second writer against the same rows — enforced neither.
//! Both paths now call [`enforce_card_update_policy`] so there is exactly
//! one policy.

/// Which fields a card update intends to touch.
///
/// Only the fields the policy actually gates are listed; construct with
/// `..Default::default()` and set the ones present in the request.
#[derive(Debug, Default, Clone, Copy)]
pub struct CardUpdateIntent {
    pub step: bool,
    pub title: bool,
    pub description: bool,
    pub priority: bool,
    pub workflow: bool,
    pub model: bool,
    pub effort: bool,
    pub blocked: bool,
    pub block_reason: bool,
    pub depends_on: bool,
}

impl CardUpdateIntent {
    /// True when the update touches something other than `step`.
    fn touches_non_step(&self) -> bool {
        self.title
            || self.description
            || self.priority
            || self.workflow
            || self.model
            || self.effort
            || self.blocked
            || self.block_reason
            || self.depends_on
    }
}

/// `done` / `wont_do` are the terminal steps of every workflow.
pub fn is_terminal_step(step: &str) -> bool {
    step == "done" || step == "wont_do"
}

/// Refuse an update that violates the terminal-state or backlog-freeze
/// invariant. `existing_step` is the card's step as read inside the
/// atomic update closure.
pub fn enforce_card_update_policy(
    existing_step: &str,
    intent: &CardUpdateIntent,
) -> anyhow::Result<()> {
    let is_terminal = is_terminal_step(existing_step);

    // Terminal cards: only step changes allowed (to reopen / move).
    // depends_on edits are also blocked in terminal states.
    if is_terminal {
        if !intent.step || intent.touches_non_step() {
            anyhow::bail!(
                "card-update-policy: card is in terminal state — only step changes allowed"
            );
        }
        return Ok(());
    }

    // description/workflow are locked once a card leaves backlog.
    // model, effort, title, priority, blocked, block_reason stay
    // editable in any non-terminal state.
    if existing_step != "backlog" && (intent.workflow || intent.description) {
        anyhow::bail!(
            "card-update-policy: description and workflow are locked after leaving backlog"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_card_allows_step_only() {
        let step_only = CardUpdateIntent {
            step: true,
            ..Default::default()
        };
        assert!(enforce_card_update_policy("done", &step_only).is_ok());
        assert!(enforce_card_update_policy("wont_do", &step_only).is_ok());

        let with_title = CardUpdateIntent {
            step: true,
            title: true,
            ..Default::default()
        };
        assert!(enforce_card_update_policy("done", &with_title).is_err());

        let no_step = CardUpdateIntent {
            title: true,
            ..Default::default()
        };
        assert!(enforce_card_update_policy("done", &no_step).is_err());
    }

    #[test]
    fn backlog_allows_description_and_workflow() {
        let intent = CardUpdateIntent {
            description: true,
            workflow: true,
            ..Default::default()
        };
        assert!(enforce_card_update_policy("backlog", &intent).is_ok());
    }

    #[test]
    fn non_backlog_freezes_description_and_workflow() {
        let wf = CardUpdateIntent {
            workflow: true,
            ..Default::default()
        };
        let desc = CardUpdateIntent {
            description: true,
            ..Default::default()
        };
        assert!(enforce_card_update_policy("in_progress", &wf).is_err());
        assert!(enforce_card_update_policy("execution", &desc).is_err());

        // Everything else stays editable mid-flight.
        let editable = CardUpdateIntent {
            title: true,
            priority: true,
            model: true,
            effort: true,
            blocked: true,
            block_reason: true,
            depends_on: true,
            step: true,
            ..Default::default()
        };
        assert!(enforce_card_update_policy("in_progress", &editable).is_ok());
    }
}
