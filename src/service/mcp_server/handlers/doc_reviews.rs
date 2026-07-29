//! The two MCP tools a document-review session gets, and nothing else:
//! read the document under review, and replace it with a revised version.
//!
//! Both resolve the review from the calling session (`ctx.session_id` →
//! `doc_reviews.session_id`), never from an argument — a session can only
//! ever touch its own review, so there is no id to spoof. `routes/mcp.rs`
//! keeps the pair out of every other session's `tools/list`; these guards
//! are what makes that stick if one calls by name anyway.

use serde_json::Value;

use super::super::McpToolRegistry;
use crate::db::models::DocReview;
use crate::service::doc_reviews;
use crate::service::github_pr;
use crate::service::mcp_server::context::ToolCallContext;

impl McpToolRegistry {
    /// `get_review_doc` — the current document + still-open annotations.
    /// The same content is injected ahead of every pass; this is the way
    /// back to it after a revision, or when the injected copy is stale.
    pub(crate) async fn handle_get_review_doc(
        &self,
        _args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let review = review_for_session(ctx).await?;
        let markdown = ctx
            .db
            .get_doc_review_version(&review.id, review.current_version)
            .await?
            .map(|v| v.markdown)
            .unwrap_or_default();
        let comments = ctx.db.list_doc_review_comments(&review.id, true).await?;

        Ok(serde_json::json!({
            "review_id": review.id,
            "title": review.title,
            "source_kind": review.source_kind,
            "version": review.current_version,
            "markdown": markdown,
            "open_comments": comments
                .iter()
                .map(|c| serde_json::json!({
                    "id": c.id,
                    "start_line": c.start_line,
                    "end_line": c.end_line,
                    "quote": c.quote,
                    "kind": c.kind,
                    "body": c.body,
                    "status": c.status,
                }))
                .collect::<Vec<_>>(),
        }))
    }

    /// `submit_review_revision` — save a new head version of the document
    /// and resolve the annotations the revision addressed.
    ///
    /// Resolutions are applied *before* the version row is written: an
    /// unknown `comment_id` must leave the review completely untouched, so
    /// the model can retry the whole call with real ids instead of stacking
    /// a second version on top of a half-applied batch.
    pub(crate) async fn handle_submit_review_revision(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let review = review_for_session(ctx).await?;

        let markdown = args
            .get("markdown")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "submit_review_revision requires non-empty 'markdown' — the COMPLETE \
                     replacement document, not a patch or an excerpt"
                )
            })?;
        let note = args
            .get("note")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("revision");

        let resolutions = parse_resolutions(args.get("resolutions"))?;
        if !resolutions.is_empty() {
            ctx.db
                .apply_comment_resolutions(&review.id, resolutions.clone())
                .await?;
            github_pr::answer_resolutions((*ctx.db).clone(), review.id.clone(), resolutions);
        }

        let version = ctx
            .db
            .insert_doc_review_version(&review.id, markdown, note, "assistant")
            .await?;

        // Back to 'annotating': the ball is with the user again — they read
        // the diff and either annotate the new version or apply it.
        ctx.db
            .set_doc_review_status(&review.id, "annotating")
            .await?;

        let event_data = serde_json::json!({
            "review_id": review.id,
            "version": version,
            "note": note,
        });
        let event = ctx
            .db
            .append_event(&ctx.session_id, "doc-review-revision", event_data.clone())
            .await?;
        ctx.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
            event_type: "event".into(),
            session_id: ctx.session_id.clone(),
            data: serde_json::json!({
                "id": event.id,
                "seq": event.seq,
                "ts": event.ts,
                "kind": "doc-review-revision",
                "data": event_data,
            }),
        });
        doc_reviews::broadcast_by_id(&ctx.db, &ctx.broadcaster, &review.id).await;

        let still_open = ctx
            .db
            .list_doc_review_comments(&review.id, true)
            .await
            .map(|c| c.len())
            .unwrap_or(0);
        Ok(serde_json::json!({
            "status": "ok",
            "review_id": review.id,
            "version": version,
            "open_comments": still_open,
            "note": if still_open == 0 {
                "Revision saved as a new version. Every annotation is resolved — tell the user what you changed."
            } else {
                "Revision saved as a new version, but some annotations are still open: resolve them in this pass or say why you left them."
            },
        }))
    }
}

/// The review this session owns, or a clear refusal. Anything that is not a
/// review session lands here — a chat that guessed the tool name, or a
/// review session whose review was deleted underneath it.
async fn review_for_session(ctx: &ToolCallContext) -> anyhow::Result<DocReview> {
    ctx.db
        .get_review_for_session(&ctx.session_id)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "this session is not bound to a document review; the review tools \
                 (get_review_doc, submit_review_revision) only work inside a review session"
            )
        })
}

/// `[{comment_id, action, note?}]` → the `(id, action, note)` triples the DB
/// layer applies as one batch. Shape errors name the offending entry: the
/// model has to be able to fix the call from the message alone.
fn parse_resolutions(
    value: Option<&Value>,
) -> anyhow::Result<Vec<(String, String, Option<String>)>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let items = value.as_array().ok_or_else(|| {
        anyhow::anyhow!("'resolutions' must be an array of {{comment_id, action, note?}} objects")
    })?;
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let comment_id = item
            .get("comment_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("resolutions[{i}] is missing 'comment_id'"))?;
        let action = item
            .get("action")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "resolutions[{i}] is missing 'action'; valid actions: {}",
                    crate::db::crud::RESOLUTION_ACTIONS.join(", ")
                )
            })?;
        let note = item
            .get("note")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        out.push((comment_id.to_string(), action.to_string(), note));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::super::McpToolRegistry;
    use crate::db::models::{NewFolder, NewSession};
    use crate::service::mcp_server::context::ToolCallContext;

    /// An in-memory DB holding a folder, a review, and (optionally) a
    /// review session bound to it, plus a `ToolCallContext` for `s1`.
    async fn ctx(bind_review: bool) -> (ToolCallContext, Arc<crate::db::Db>, String) {
        let db = Arc::new(crate::db::Db::in_memory().unwrap());
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
            name: "Review".into(),
            folder_id: "f1".into(),
            is_expert: true,
            expert_kind: Some(crate::service::doc_reviews::EXPERT_KIND.into()),
            created_at: ts.clone(),
            last_activity: ts,
            ..Default::default()
        })
        .await
        .unwrap();
        let review = db
            .create_doc_review(
                "Doc",
                "file",
                "f1:docs/doc.md",
                Some("f1"),
                None,
                "# Doc\n\nline two\n",
            )
            .await
            .unwrap();
        if bind_review {
            db.set_doc_review_session(&review.id, Some("s1"))
                .await
                .unwrap();
        }
        let ctx = ToolCallContext {
            session_id: "s1".into(),
            project_id: None,
            card_id: None,
            db: db.clone(),
            broadcaster: crate::ws::broadcaster::Broadcaster::new(),
            provider_registry: None,
            data_dir: None,
            folder_id: "f1".into(),
        };
        (ctx, db, review.id)
    }

    #[tokio::test]
    async fn both_tools_refuse_a_session_with_no_review() {
        let (ctx, _db, _id) = ctx(false).await;
        let registry = McpToolRegistry::new();
        let err = registry
            .handle_get_review_doc(serde_json::json!({}), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not bound to a document review"), "got: {err}");
        let err = registry
            .handle_submit_review_revision(
                serde_json::json!({ "markdown": "# New", "note": "n" }),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not bound to a document review"), "got: {err}");
    }

    #[tokio::test]
    async fn get_review_doc_returns_head_markdown_and_open_comments() {
        let (ctx, db, id) = ctx(true).await;
        let open = db
            .add_doc_review_comment(&id, 1, (3, 3), Some("line two"), "suggest", "tighten")
            .await
            .unwrap();
        let resolved = db
            .add_doc_review_comment(&id, 1, (1, 1), None, "comment", "nice")
            .await
            .unwrap();
        db.apply_comment_resolutions(&id, vec![(resolved.id.clone(), "fixed".into(), None)])
            .await
            .unwrap();

        let out = McpToolRegistry::new()
            .handle_get_review_doc(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert_eq!(out["review_id"], id);
        assert_eq!(out["version"], 1);
        assert_eq!(out["markdown"], "# Doc\n\nline two\n");
        let comments = out["open_comments"].as_array().unwrap();
        assert_eq!(
            comments.len(),
            1,
            "resolved annotations are not open: {out}"
        );
        assert_eq!(comments[0]["id"], open.id);
        assert_eq!(comments[0]["quote"], "line two");
        assert_eq!(comments[0]["kind"], "suggest");
    }

    #[tokio::test]
    async fn revision_bumps_the_version_applies_resolutions_and_logs_an_event() {
        let (ctx, db, id) = ctx(true).await;
        let comment = db
            .add_doc_review_comment(&id, 1, (3, 3), None, "wrong", "the port is 8080")
            .await
            .unwrap();
        db.set_doc_review_status(&id, "running").await.unwrap();

        let out = McpToolRegistry::new()
            .handle_submit_review_revision(
                serde_json::json!({
                    "markdown": "# Doc\n\nport 8080\n",
                    "note": "fixed the port",
                    "resolutions": [
                        { "comment_id": comment.id, "action": "fixed", "note": "was 80" }
                    ],
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out["version"], 2, "got: {out}");
        assert_eq!(out["open_comments"], 0);

        let review = db.get_doc_review(&id).await.unwrap().unwrap();
        assert_eq!(review.current_version, 2);
        assert_eq!(
            review.status, "annotating",
            "the ball is back with the user"
        );
        let head = db.get_doc_review_version(&id, 2).await.unwrap().unwrap();
        assert_eq!(head.markdown, "# Doc\n\nport 8080\n");
        assert_eq!(head.created_by, "assistant");
        assert_eq!(head.note, "fixed the port");
        let comments = db.list_doc_review_comments(&id, false).await.unwrap();
        assert_eq!(comments[0].status, "fixed");
        assert_eq!(comments[0].resolution_note.as_deref(), Some("was 80"));

        let events = db.list_events_by_session("s1", None).await.unwrap();
        let ev = events
            .iter()
            .find(|e| e.kind == "doc-review-revision")
            .expect("a doc-review-revision event");
        let data: serde_json::Value = serde_json::from_str(&ev.data).unwrap();
        assert_eq!(data["review_id"], id);
        assert_eq!(data["version"], 2);
        assert_eq!(data["note"], "fixed the port");
    }

    #[tokio::test]
    async fn an_unknown_comment_id_writes_nothing_and_lists_the_valid_ids() {
        let (ctx, db, id) = ctx(true).await;
        let comment = db
            .add_doc_review_comment(&id, 1, (1, 1), None, "comment", "hm")
            .await
            .unwrap();

        let err = McpToolRegistry::new()
            .handle_submit_review_revision(
                serde_json::json!({
                    "markdown": "# Doc\n\nrevised\n",
                    "note": "n",
                    "resolutions": [{ "comment_id": "nope", "action": "fixed" }],
                }),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown comment_id"), "got: {err}");
        assert!(
            err.contains(&comment.id),
            "the error names the valid ids: {err}"
        );

        let review = db.get_doc_review(&id).await.unwrap().unwrap();
        assert_eq!(review.current_version, 1, "no version was written");
        assert_eq!(
            db.list_doc_review_comments(&id, false).await.unwrap()[0].status,
            "pending"
        );
    }

    #[tokio::test]
    async fn an_empty_revision_is_refused() {
        let (ctx, db, id) = ctx(true).await;
        let err = McpToolRegistry::new()
            .handle_submit_review_revision(
                serde_json::json!({ "markdown": "   ", "note": "n" }),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("non-empty 'markdown'"), "got: {err}");
        assert_eq!(
            db.get_doc_review(&id)
                .await
                .unwrap()
                .unwrap()
                .current_version,
            1
        );
    }

    #[tokio::test]
    async fn a_revision_leaves_an_unresolved_annotation_on_its_own_passage() {
        let (ctx, db, id) = ctx(true).await;
        let comment = db
            .add_doc_review_comment(&id, 1, (3, 3), Some("line two"), "suggest", "expand this")
            .await
            .unwrap();
        let registry = McpToolRegistry::new();

        // A revision that answers something else, and in doing so pushes
        // the annotated passage two lines down.
        registry
            .handle_submit_review_revision(
                serde_json::json!({
                    "markdown": "# Doc\n\nA new opening.\n\nline two\n",
                    "note": "added an opener",
                }),
                &ctx,
            )
            .await
            .unwrap();

        // The next pass reads the document back, and the annotation still
        // names the lines the words it was written about now live on.
        let out = registry
            .handle_get_review_doc(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        let open = &out["open_comments"][0];
        assert_eq!(open["id"], comment.id);
        assert_eq!(open["start_line"], 5, "got: {out}");
        assert_eq!(open["end_line"], 5);
        assert_eq!(
            out["markdown"].as_str().unwrap().lines().nth(4),
            Some("line two")
        );
    }
}
