//! The AI half of Document Review: the identity of a review's dedicated
//! session (`expert_kind` + system prompt) and the small helpers every
//! surface that moves a review shares — the HTTP pass endpoint, the MCP
//! tool handlers, and the ask/answer hooks all have to broadcast the same
//! `doc-review-update` and flip the same statuses.
//!
//! The document adapters (read/write the underlying file, report, or plan)
//! live next door in [`crate::service::doc_review_sources`]; nothing here
//! touches disk.

use crate::db::Db;
use crate::db::models::DocReview;
use crate::ws::broadcaster::{Broadcaster, WsEvent};

/// `sessions.expert_kind` of a review's AI session. The MCP layer keys the
/// review-only tools off this value, so it must match what the pass
/// endpoint stamps on the session it creates.
pub const EXPERT_KIND: &str = "doc-review";

/// The review session's system prompt — the whole behaviour contract of the
/// feature. It is deliberately explicit about the two mistakes that would
/// destroy a document: revising with a partial `markdown` (everything
/// omitted is deleted), and editing the source file directly (the user, not
/// the assistant, decides when a version is applied).
pub const SYSTEM_PROMPT: &str = "\
You are PeckBoard's document-review assistant. You work on ONE markdown \
document with one user, over as many passes as they want.

Every turn begins with a one-shot block holding the document's title, its \
source, the current version number, the FULL markdown with 1-based line \
numbers, and every open annotation the user has left. The line numbers are \
an addressing aid for the annotations — never copy them into the document.

REVISING
- The ONLY way to change the document is the `submit_review_revision` tool. \
Never edit the source file on disk (no write_file / edit_file / run_command \
on it, no matter how convenient): the user applies an approved version \
themselves, and unlimited passes only work because the source is untouched \
until then.
- `markdown` must be the COMPLETE replacement document, never a patch, a \
diff, a fragment, or a summary. Anything you leave out is deleted.
- Change only what the open annotations — or the user's message — ask for. \
Every other sentence, heading, list item, code block, link, and blank line \
must come back byte-identical. The user reads your revision as a diff, so \
incidental rewording buries the change they asked for.
- Keep the document's voice, formatting conventions, and any front matter.
- `note` is one short line naming what changed (\"tightened the intro, fixed \
the port number\"); it is what the version history shows.

RESOLVING ANNOTATIONS
- Address EVERY open annotation in the pass and report each one in \
`resolutions`, by id:
  - `fixed` — you changed the document to satisfy it.
  - `declined` — you deliberately did not; say why in the note.
  - `answered` — it was a question rather than an edit; put the answer in \
the note.
- The annotation kinds mean: `comment` a general remark, `suggest` a \
specific edit, `wrong` a factual error (verify it before rewriting), \
`expand` more detail, `shorten` tighten it without dropping meaning.
- An annotation you leave out stays open and comes back on the next pass.

WHEN YOU ARE UNSURE
- If an annotation could reasonably mean two different edits, or satisfying \
it would mean inventing facts, ask with `ask_user` instead of guessing. Ask \
the specific question, offer the options you can see, and wait — one round \
of questions beats three passes of wrong rewrites.
- Call `get_review_doc` any time you need the current document and open \
annotations again (after your own revision, or if the injected copy looks \
stale).

CHAT VERSUS REVISION
- A message that only asks something (\"why is this section here?\", \"does \
this cover the failure case?\") gets a plain answer in chat — do NOT revise \
the document for it.
- Revise when the user asks for a change, or when open annotations are \
handed to you.
- Keep chat answers short, concrete, and about this document.";

/// Tell every connected client that a review moved. Keyed by the review id
/// (not a session id) so the review screen subscribes to it the same way a
/// session subscribes to its own stream.
pub fn broadcast_update(broadcaster: &Broadcaster, review: &DocReview) {
    broadcaster.broadcast(WsEvent {
        event_type: "doc-review-update".into(),
        session_id: review.id.clone(),
        data: serde_json::json!({
            "review_id": review.id,
            "version": review.current_version,
            "status": review.status,
        }),
    });
}

/// Re-read a review and broadcast its (possibly changed) head + status.
pub async fn broadcast_by_id(db: &Db, broadcaster: &Broadcaster, review_id: &str) {
    if let Ok(Some(review)) = db.get_doc_review(review_id).await {
        broadcast_update(broadcaster, &review);
    }
}

/// Move a review to `status` and broadcast the result. Errors are logged
/// and swallowed: every caller is a side effect of work that has already
/// happened (a revision landed, a question was asked), so a failed status
/// write must not fail the caller.
pub async fn set_status(db: &Db, broadcaster: &Broadcaster, review_id: &str, status: &str) {
    if let Err(e) = db.set_doc_review_status(review_id, status).await {
        tracing::warn!(review_id, status, "doc review status update failed: {e}");
        return;
    }
    broadcast_by_id(db, broadcaster, review_id).await;
}

/// The review a session is bound to, but ONLY for a genuine review session
/// (`expert_kind == "doc-review"`). Every hook that reacts to a generic
/// session event — a question asked, a question answered — goes through
/// this so an ordinary chat that happens to share an id shape can never
/// move a review's status.
pub async fn review_for_review_session(db: &Db, session_id: &str) -> Option<DocReview> {
    let session = db.get_session(session_id).await.ok().flatten()?;
    if session.expert_kind.as_deref() != Some(EXPERT_KIND) {
        return None;
    }
    db.get_review_for_session(session_id).await.ok().flatten()
}

/// The review session asked the user something: the pass is parked until
/// they answer, so the review shows a question instead of a spinner. A
/// review the user already approved is left alone.
pub async fn mark_needs_input(db: &Db, broadcaster: &Broadcaster, session_id: &str) {
    if let Some(review) = review_for_review_session(db, session_id).await
        && review.status != "approved"
    {
        set_status(db, broadcaster, &review.id, "needs_input").await;
    }
}

/// The question was answered, so the turn resumes and the review is running
/// again. Only from `needs_input`: an answer that arrives after the pass
/// already finished (or was applied) must not drag the review back into a
/// spinner.
pub async fn resume_after_question(db: &Db, broadcaster: &Broadcaster, session_id: &str) {
    if let Some(review) = review_for_review_session(db, session_id).await
        && review.status == "needs_input"
    {
        set_status(db, broadcaster, &review.id, "running").await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{NewFolder, NewSession};

    /// A folder, a review, and a session of `expert_kind` bound to it.
    async fn seed(expert_kind: Option<&str>) -> (Db, String) {
        let db = Db::in_memory().unwrap();
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_session(NewSession {
            id: "s1".into(),
            name: "S".into(),
            folder_id: "f1".into(),
            is_expert: expert_kind.is_some(),
            expert_kind: expert_kind.map(str::to_string),
            created_at: ts.clone(),
            last_activity: ts,
            ..Default::default()
        })
        .await
        .unwrap();
        let review = db
            .create_doc_review("Doc", "file", "f1:doc.md", Some("f1"), None, "# Doc\n")
            .await
            .unwrap();
        db.set_doc_review_session(&review.id, Some("s1"))
            .await
            .unwrap();
        (db, review.id)
    }

    #[tokio::test]
    async fn question_hooks_walk_running_to_needs_input_and_back() {
        let (db, id) = seed(Some(EXPERT_KIND)).await;
        let bc = Broadcaster::new();
        db.set_doc_review_status(&id, "running").await.unwrap();

        mark_needs_input(&db, &bc, "s1").await;
        assert_eq!(
            db.get_doc_review(&id).await.unwrap().unwrap().status,
            "needs_input"
        );

        resume_after_question(&db, &bc, "s1").await;
        assert_eq!(
            db.get_doc_review(&id).await.unwrap().unwrap().status,
            "running"
        );
    }

    #[tokio::test]
    async fn an_answered_question_never_reopens_a_settled_review() {
        let (db, id) = seed(Some(EXPERT_KIND)).await;
        let bc = Broadcaster::new();
        db.set_doc_review_status(&id, "approved").await.unwrap();

        mark_needs_input(&db, &bc, "s1").await;
        resume_after_question(&db, &bc, "s1").await;
        assert_eq!(
            db.get_doc_review(&id).await.unwrap().unwrap().status,
            "approved",
            "an approved review is not dragged back into a pass"
        );
    }

    #[tokio::test]
    async fn a_plain_chat_session_cannot_move_a_review() {
        let (db, id) = seed(None).await;
        let bc = Broadcaster::new();
        db.set_doc_review_status(&id, "running").await.unwrap();
        mark_needs_input(&db, &bc, "s1").await;
        assert_eq!(
            db.get_doc_review(&id).await.unwrap().unwrap().status,
            "running",
            "the expert_kind guard holds even when the session owns the review"
        );
    }
}
