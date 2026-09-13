//! Resuming a provider conversation, and the proof that the id being
//! resumed belongs to the provider about to be dispatched.
//!
//! Every CLI-backed provider persists its conversations locally, keyed by an
//! opaque id it hands back on `agent-start` / `agent-end`: Claude's session
//! uuid, Codex's thread uuid (a rollout file), Grok's `sessionId`, Kimi's
//! `session_id`, Cursor's chat id. Peckboard stores the newest one on
//! `sessions.conversation_id` and passes it back on the next turn so the CLI
//! resumes instead of starting cold.
//!
//! Those ids are all opaque uuids, from the same field name, written to the
//! same column — so nothing about their *shape* stops one provider's id from
//! reaching another provider's `--resume`. When that happens the CLI fails
//! the spawn outright and keeps failing, because the id it was handed names
//! a conversation that does not exist in its store and never will
//! (`no rollout found for thread id …` from Codex; `No conversation found
//! with session ID …` from Claude). The turn cannot succeed, and a retry
//! re-derives the same id, so the session is wedged until someone clears it.
//!
//! [`ResumeHandle`] closes that off in the type system: a provider takes a
//! handle, not a `String`, and the only constructor checks the id's owner
//! against the dispatch target first.

use crate::handover::continuity_key;

/// An id a provider may resume, carrying the proof that it can.
///
/// Proof token: bearer has verified this id was produced by a run whose
/// model shares the dispatch target's continuity key — same provider, same
/// account. See `SessionManager::resolve_resume_handle` for an example.
///
/// Holding one is the only way to reach a provider's resume path:
/// [`crate::provider::agent::SendMessageContext::conversation_id`] is an
/// `Option<ResumeHandle>`, so a bare id read out of the session row or
/// scanned off the event log cannot be dispatched without passing through
/// [`ResumeHandle::for_model`] — which is where the check lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeHandle {
    id: String,
}

impl ResumeHandle {
    /// The only constructor. `Some` when `owner_model` — the model of the
    /// run that produced `id` — shares `target_model`'s continuity key, so
    /// the CLI about to be spawned is the one that wrote the conversation.
    /// `None` on a provider or account change: the incoming CLI cannot see
    /// the outgoing one's store (Codex rollouts and Claude sessions live in
    /// different directories; two Claude accounts get different
    /// `CLAUDE_CONFIG_DIR`s), so the only correct move is a cold start.
    ///
    /// An empty / whitespace-only id is `None` — some providers emit the
    /// field with no value on a run that never established a conversation.
    pub fn for_model(
        id: impl Into<String>,
        owner_model: &str,
        target_model: &str,
    ) -> Option<ResumeHandle> {
        let id = id.into();
        if id.trim().is_empty() {
            return None;
        }
        if continuity_key(owner_model) != continuity_key(target_model) {
            tracing::debug!(
                owner = %owner_model,
                target = %target_model,
                "Refusing to resume a conversation across a continuity boundary"
            );
            return None;
        }
        Some(ResumeHandle { id })
    }

    /// The raw id, for the CLI argument the provider builds.
    pub fn id(&self) -> &str {
        &self.id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_provider_and_account_resumes() {
        let h = ResumeHandle::for_model("conv-1", "claude:opus", "claude:sonnet");
        assert_eq!(h.map(|h| h.id().to_string()), Some("conv-1".into()));
    }

    #[test]
    fn bare_ids_match_the_default_provider() {
        // Legacy rows store a prefix-less model; it means claude.
        assert!(ResumeHandle::for_model("conv-1", "opus", "claude:opus").is_some());
    }

    #[test]
    fn provider_change_refuses_to_resume() {
        // The incident this exists for: a Claude session uuid reaching
        // `codex exec resume`, which fails with "no rollout found".
        assert_eq!(
            ResumeHandle::for_model(
                "365eac68-b3db-4b63-91d8-414674557d5a",
                "claude:claude-opus-5@acc_1",
                "codex:gpt-6-astra@cacc_1",
            ),
            None
        );
    }

    #[test]
    fn account_change_refuses_to_resume() {
        assert_eq!(
            ResumeHandle::for_model("conv-1", "claude:opus@acc_1", "claude:opus@acc_2"),
            None
        );
        // Gaining an account is a change too — the CLI's config dir moves.
        assert_eq!(
            ResumeHandle::for_model("conv-1", "claude:opus", "claude:opus@acc_2"),
            None
        );
    }

    #[test]
    fn blank_id_is_never_a_handle() {
        assert_eq!(
            ResumeHandle::for_model("", "claude:opus", "claude:opus"),
            None
        );
        assert_eq!(
            ResumeHandle::for_model("   ", "claude:opus", "claude:opus"),
            None
        );
    }
}
