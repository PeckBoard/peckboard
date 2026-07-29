//! Document Review persistence: reviews, their versioned markdown, and the
//! per-line annotations a user attaches to them.
//!
//! The document text is never stored on the review row — every revision is
//! an immutable `doc_review_versions` row and `doc_reviews.current_version`
//! points at the head. That is what makes "unlimited passes, always a diff,
//! never a silent overwrite" a property of the schema rather than of the
//! code that happens to write it.

use diesel::prelude::*;

use crate::db::Db;
use crate::db::crud::doc_review_anchors::line_map;
use crate::db::models::*;
use crate::db::schema::*;

/// Terminal statuses a review session may set on a comment when it resolves
/// it. Anything else is rejected before touching the DB.
pub const RESOLUTION_ACTIONS: [&str; 3] = ["fixed", "declined", "answered"];

/// Comment statuses that still need the assistant's attention — the set the
/// injection block and the annotation rail call "open".
const OPEN_COMMENT_STATUSES: [&str; 2] = ["pending", "sent"];

impl Db {
    /// Create a review together with version 1 of its document, atomically.
    /// A review without a version row would have nothing to render, so the
    /// two inserts share one connection lock.
    pub async fn create_doc_review(
        &self,
        title: &str,
        source_kind: &str,
        source_ref: &str,
        folder_id: Option<&str>,
        project_id: Option<&str>,
        markdown: &str,
    ) -> anyhow::Result<DocReview> {
        let now = chrono::Utc::now().to_rfc3339();
        let review = NewDocReview {
            id: uuid::Uuid::new_v4().to_string(),
            title: title.to_string(),
            source_kind: source_kind.to_string(),
            source_ref: source_ref.to_string(),
            folder_id: folder_id.map(str::to_string),
            project_id: project_id.map(str::to_string),
            session_id: None,
            status: "annotating".into(),
            current_version: 1,
            created_at: now.clone(),
            updated_at: now.clone(),
        };
        let version = NewDocReviewVersion {
            review_id: review.id.clone(),
            version: 1,
            markdown: markdown.to_string(),
            note: "initial".into(),
            created_by: "user".into(),
            created_at: now,
        };
        self.with_conn(move |conn| {
            let row = diesel::insert_into(doc_reviews::table)
                .values(&review)
                .returning(DocReview::as_returning())
                .get_result(conn)?;
            diesel::insert_into(doc_review_versions::table)
                .values(&version)
                .execute(conn)?;
            Ok(row)
        })
        .await
    }

    /// Fetch one review by id.
    pub async fn get_doc_review(&self, id: &str) -> anyhow::Result<Option<DocReview>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            doc_reviews::table
                .find(&id)
                .select(DocReview::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// The review a session was created for. How an MCP handler resolves
    /// "which document am I reviewing?" from `ctx.session_id`.
    pub async fn get_review_for_session(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Option<DocReview>> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            doc_reviews::table
                .filter(doc_reviews::session_id.eq(&session_id))
                .order(doc_reviews::updated_at.desc())
                .select(DocReview::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Every review, most recently touched first.
    pub async fn list_doc_reviews(&self) -> anyhow::Result<Vec<DocReview>> {
        self.with_conn(move |conn| {
            doc_reviews::table
                .order(doc_reviews::updated_at.desc())
                .select(DocReview::as_select())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Delete a review. Versions and comments go with it via the SQLite
    /// `ON DELETE CASCADE` on their `review_id` FKs; `user_tabs` is
    /// polymorphic with no FK, so its rows are cleaned explicitly (an
    /// orphan tab chip renders with no resolvable title).
    pub async fn delete_doc_review(&self, id: &str) -> anyhow::Result<usize> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            diesel::delete(
                user_tabs::table
                    .filter(user_tabs::item_type.eq("doc_review"))
                    .filter(user_tabs::item_id.eq(&id)),
            )
            .execute(conn)?;
            diesel::delete(doc_reviews::table.find(&id))
                .execute(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Set a review's lifecycle status
    /// (`annotating` | `running` | `needs_input` | `approved`).
    pub async fn set_doc_review_status(&self, id: &str, status: &str) -> anyhow::Result<()> {
        let id = id.to_string();
        let status = status.to_string();
        let now = chrono::Utc::now().to_rfc3339();
        self.with_conn(move |conn| {
            diesel::update(doc_reviews::table.find(&id))
                .set((
                    doc_reviews::status.eq(&status),
                    doc_reviews::updated_at.eq(&now),
                ))
                .execute(conn)?;
            Ok(())
        })
        .await
    }

    /// Attach (or detach, with `None`) the review AI session.
    pub async fn set_doc_review_session(
        &self,
        id: &str,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let id = id.to_string();
        let session_id = session_id.map(str::to_string);
        let now = chrono::Utc::now().to_rfc3339();
        self.with_conn(move |conn| {
            diesel::update(doc_reviews::table.find(&id))
                .set((
                    doc_reviews::session_id.eq(session_id),
                    doc_reviews::updated_at.eq(&now),
                ))
                .execute(conn)?;
            Ok(())
        })
        .await
    }

    /// Append a new version of the document and move the head pointer to
    /// it. Returns the new version number. `created_by` is `user` (initial
    /// load, revert) or `assistant` (a revision from a review pass).
    pub async fn insert_doc_review_version(
        &self,
        review_id: &str,
        markdown: &str,
        note: &str,
        created_by: &str,
    ) -> anyhow::Result<i32> {
        let review_id = review_id.to_string();
        let markdown = markdown.to_string();
        let note = note.to_string();
        let created_by = created_by.to_string();
        let now = chrono::Utc::now().to_rfc3339();
        self.with_conn(move |conn| {
            // Read the head under the same connection lock that writes the
            // new row, so two concurrent revisions can't pick the same
            // version number.
            let current: Option<i32> = doc_reviews::table
                .find(&review_id)
                .select(doc_reviews::current_version)
                .first(conn)
                .optional()?;
            let Some(current) = current else {
                anyhow::bail!("doc review not found: {review_id}");
            };
            let previous: Option<String> = doc_review_versions::table
                .filter(doc_review_versions::review_id.eq(&review_id))
                .filter(doc_review_versions::version.eq(current))
                .select(doc_review_versions::markdown)
                .first(conn)
                .optional()?;
            let next = current + 1;
            diesel::insert_into(doc_review_versions::table)
                .values(&NewDocReviewVersion {
                    review_id: review_id.clone(),
                    version: next,
                    markdown: markdown.clone(),
                    note,
                    created_by,
                    created_at: now.clone(),
                })
                .execute(conn)?;
            // The revision just moved the lines every annotation hangs off.
            // Remap them inside the same lock, so no reader can catch the
            // new document sitting beside the old document's anchors.
            if let Some(previous) = previous.filter(|p| *p != markdown) {
                remap_comments(conn, &review_id, next, &previous, &markdown)?;
            }
            diesel::update(doc_reviews::table.find(&review_id))
                .set((
                    doc_reviews::current_version.eq(next),
                    doc_reviews::updated_at.eq(&now),
                ))
                .execute(conn)?;
            Ok(next)
        })
        .await
    }

    /// One version of a document, markdown included.
    pub async fn get_doc_review_version(
        &self,
        review_id: &str,
        version: i32,
    ) -> anyhow::Result<Option<DocReviewVersion>> {
        let review_id = review_id.to_string();
        self.with_conn(move |conn| {
            doc_review_versions::table
                .filter(doc_review_versions::review_id.eq(&review_id))
                .filter(doc_review_versions::version.eq(version))
                .select(DocReviewVersion::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Version history, newest first, **without** the markdown bodies — the
    /// history list only needs note/author/timestamp, and shipping every
    /// body would send the whole document once per revision.
    pub async fn list_doc_review_versions(
        &self,
        review_id: &str,
    ) -> anyhow::Result<Vec<DocReviewVersionMeta>> {
        let review_id = review_id.to_string();
        self.with_conn(move |conn| {
            doc_review_versions::table
                .filter(doc_review_versions::review_id.eq(&review_id))
                .order(doc_review_versions::version.desc())
                .select(DocReviewVersionMeta::as_select())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Annotations on a review. `open_only` keeps just the ones still
    /// awaiting the assistant (`pending` / `sent`).
    pub async fn list_doc_review_comments(
        &self,
        review_id: &str,
        open_only: bool,
    ) -> anyhow::Result<Vec<DocReviewComment>> {
        let review_id = review_id.to_string();
        self.with_conn(move |conn| {
            let mut q = doc_review_comments::table
                .filter(doc_review_comments::review_id.eq(&review_id))
                .into_boxed();
            if open_only {
                q = q.filter(doc_review_comments::status.eq_any(OPEN_COMMENT_STATUSES));
            }
            q.order((
                doc_review_comments::start_line.asc(),
                doc_review_comments::created_at.asc(),
            ))
            .select(DocReviewComment::as_select())
            .load(conn)
            .map_err(Into::into)
        })
        .await
    }

    /// Add an annotation anchored to `lines` — a 1-based inclusive
    /// `(start, end)` range in the given version. New comments always start
    /// `pending`.
    pub async fn add_doc_review_comment(
        &self,
        review_id: &str,
        version: i32,
        lines: (i32, i32),
        quote: Option<&str>,
        kind: &str,
        body: &str,
    ) -> anyhow::Result<DocReviewComment> {
        let (start_line, end_line) = lines;
        let new = NewDocReviewComment {
            id: uuid::Uuid::new_v4().to_string(),
            review_id: review_id.to_string(),
            version,
            start_line,
            end_line,
            quote: quote.map(str::to_string),
            kind: kind.to_string(),
            body: body.to_string(),
            status: "pending".into(),
            resolution_note: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            external_kind: None,
            external_id: None,
        };
        self.with_conn(move |conn| {
            diesel::insert_into(doc_review_comments::table)
                .values(&new)
                .returning(DocReviewComment::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Insert an annotation that came from somewhere else — today, a GitHub
    /// pull-request review comment. Idempotent on `(review_id,
    /// external_id)`: a re-sync of a PR that has said nothing new is a
    /// no-op, not a second copy of every thread.
    ///
    /// Returns the annotation, and whether this call is what created it.
    pub async fn import_doc_review_comment(
        &self,
        review_id: &str,
        version: i32,
        lines: (i32, i32),
        quote: Option<&str>,
        kind: &str,
        body: &str,
        external_kind: &str,
        external_id: &str,
    ) -> anyhow::Result<(DocReviewComment, bool)> {
        let (start_line, end_line) = lines;
        let new = NewDocReviewComment {
            id: uuid::Uuid::new_v4().to_string(),
            review_id: review_id.to_string(),
            version,
            start_line,
            end_line,
            quote: quote.map(str::to_string),
            kind: kind.to_string(),
            body: body.to_string(),
            status: "pending".into(),
            resolution_note: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            external_kind: Some(external_kind.to_string()),
            external_id: Some(external_id.to_string()),
        };
        let review_id = review_id.to_string();
        let external_id = external_id.to_string();
        self.with_conn(move |conn| {
            // Look under the same lock that inserts, so two syncs racing on
            // one review can't both decide the comment is new.
            let existing: Option<DocReviewComment> = doc_review_comments::table
                .filter(doc_review_comments::review_id.eq(&review_id))
                .filter(doc_review_comments::external_id.eq(&external_id))
                .select(DocReviewComment::as_select())
                .first(conn)
                .optional()?;
            if let Some(existing) = existing {
                return Ok((existing, false));
            }
            let inserted = diesel::insert_into(doc_review_comments::table)
                .values(&new)
                .returning(DocReviewComment::as_returning())
                .get_result(conn)?;
            Ok((inserted, true))
        })
        .await
    }

    /// The pull request this review is tied to, if any.
    pub async fn get_doc_review_pr_link(
        &self,
        review_id: &str,
    ) -> anyhow::Result<Option<DocReviewPrLink>> {
        let review_id = review_id.to_string();
        self.with_conn(move |conn| {
            doc_review_pr_links::table
                .find(&review_id)
                .select(DocReviewPrLink::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Tie a review to a pull request, replacing any existing link. One link
    /// per review — re-linking is how you point it somewhere else.
    pub async fn set_doc_review_pr_link(
        &self,
        review_id: &str,
        owner: &str,
        repo: &str,
        number: i32,
        file_path: &str,
    ) -> anyhow::Result<DocReviewPrLink> {
        let new = NewDocReviewPrLink {
            review_id: review_id.to_string(),
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
            file_path: file_path.to_string(),
            last_synced_at: None,
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        self.with_conn(move |conn| {
            diesel::insert_into(doc_review_pr_links::table)
                .values(&new)
                .on_conflict(doc_review_pr_links::review_id)
                .do_update()
                .set((
                    doc_review_pr_links::owner.eq(&new.owner),
                    doc_review_pr_links::repo.eq(&new.repo),
                    doc_review_pr_links::number.eq(new.number),
                    doc_review_pr_links::file_path.eq(&new.file_path),
                ))
                .returning(DocReviewPrLink::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Drop the link. The annotations it imported stay — they were read and
    /// answered like any other, and deleting them would rewrite history.
    pub async fn clear_doc_review_pr_link(&self, review_id: &str) -> anyhow::Result<usize> {
        let review_id = review_id.to_string();
        self.with_conn(move |conn| {
            diesel::delete(doc_review_pr_links::table.find(&review_id))
                .execute(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Stamp a successful sync, so the UI can say when it last looked.
    pub async fn touch_doc_review_pr_sync(&self, review_id: &str) -> anyhow::Result<()> {
        let review_id = review_id.to_string();
        let now = chrono::Utc::now().to_rfc3339();
        self.with_conn(move |conn| {
            diesel::update(doc_review_pr_links::table.find(&review_id))
                .set(doc_review_pr_links::last_synced_at.eq(&now))
                .execute(conn)?;
            Ok(())
        })
        .await
    }

    /// Edit an annotation. Every field is optional; `None` leaves it as-is.
    /// Returns `None` when no comment with that id exists.
    pub async fn update_doc_review_comment(
        &self,
        id: &str,
        body: Option<&str>,
        kind: Option<&str>,
        status: Option<&str>,
        resolution_note: Option<&str>,
    ) -> anyhow::Result<Option<DocReviewComment>> {
        let id = id.to_string();
        let body = body.map(str::to_string);
        let kind = kind.map(str::to_string);
        let status = status.map(str::to_string);
        let resolution_note = resolution_note.map(str::to_string);
        self.with_conn(move |conn| {
            // Diesel rejects an UPDATE with an empty SET clause, so a
            // no-op patch short-circuits to a plain read.
            let any_change =
                body.is_some() || kind.is_some() || status.is_some() || resolution_note.is_some();
            if any_change {
                let target = doc_review_comments::table.find(&id);
                if let Some(v) = body {
                    diesel::update(target)
                        .set(doc_review_comments::body.eq(v))
                        .execute(conn)?;
                }
                if let Some(v) = kind {
                    diesel::update(target)
                        .set(doc_review_comments::kind.eq(v))
                        .execute(conn)?;
                }
                if let Some(v) = status {
                    diesel::update(target)
                        .set(doc_review_comments::status.eq(v))
                        .execute(conn)?;
                }
                if let Some(v) = resolution_note {
                    diesel::update(target)
                        .set(doc_review_comments::resolution_note.eq(v))
                        .execute(conn)?;
                }
            }
            doc_review_comments::table
                .find(&id)
                .select(DocReviewComment::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Delete a single annotation.
    pub async fn delete_doc_review_comment(&self, id: &str) -> anyhow::Result<usize> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            diesel::delete(doc_review_comments::table.find(&id))
                .execute(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Flip every `pending` annotation to `sent` — called when a pass hands
    /// them to the review session. Comments queued *during* a run stay
    /// `pending` and ride along on the next pass.
    pub async fn mark_pending_comments_sent(&self, review_id: &str) -> anyhow::Result<usize> {
        let review_id = review_id.to_string();
        self.with_conn(move |conn| {
            diesel::update(
                doc_review_comments::table
                    .filter(doc_review_comments::review_id.eq(&review_id))
                    .filter(doc_review_comments::status.eq("pending")),
            )
            .set(doc_review_comments::status.eq("sent"))
            .execute(conn)
            .map_err(Into::into)
        })
        .await
    }

    /// Apply the resolutions a revision reports, as one batch. Every
    /// `comment_id` must belong to `review_id` and every action must be one
    /// of [`RESOLUTION_ACTIONS`]; otherwise nothing is written and the error
    /// names the valid ids, so the model can retry with real ones instead of
    /// silently losing half a batch.
    pub async fn apply_comment_resolutions(
        &self,
        review_id: &str,
        resolutions: Vec<(String, String, Option<String>)>,
    ) -> anyhow::Result<usize> {
        if let Some((id, action, _)) = resolutions
            .iter()
            .find(|(_, action, _)| !RESOLUTION_ACTIONS.contains(&action.as_str()))
        {
            anyhow::bail!(
                "invalid resolution action {action:?} for comment {id}; valid actions: {}",
                RESOLUTION_ACTIONS.join(", ")
            );
        }
        let review_id = review_id.to_string();
        self.with_conn(move |conn| {
            let valid: Vec<String> = doc_review_comments::table
                .filter(doc_review_comments::review_id.eq(&review_id))
                .select(doc_review_comments::id)
                .load(conn)?;
            let unknown: Vec<&str> = resolutions
                .iter()
                .map(|(id, _, _)| id.as_str())
                .filter(|id| !valid.iter().any(|v| v == id))
                .collect();
            if !unknown.is_empty() {
                anyhow::bail!(
                    "unknown comment_id(s): {}; valid ids for this review: {}",
                    unknown.join(", "),
                    valid.join(", ")
                );
            }
            let mut applied = 0usize;
            for (id, action, note) in &resolutions {
                applied += diesel::update(doc_review_comments::table.find(id))
                    .set((
                        doc_review_comments::status.eq(action),
                        doc_review_comments::resolution_note.eq(note.clone()),
                    ))
                    .execute(conn)?;
            }
            Ok(applied)
        })
        .await
    }

    /// Arm the one-shot review injection on a session: its next turn gets
    /// the document and open comments prepended (see `crate::handover`).
    pub async fn set_pending_doc_review(
        &self,
        session_id: &str,
        review_id: &str,
    ) -> anyhow::Result<()> {
        let session_id = session_id.to_string();
        let review_id = review_id.to_string();
        self.with_conn(move |conn| {
            diesel::update(sessions::table.find(&session_id))
                .set(sessions::pending_doc_review.eq(Some(review_id)))
                .execute(conn)?;
            Ok(())
        })
        .await
    }

    /// Read **and clear** the pending review id in one step: the flag is
    /// one-shot, so a read that didn't clear it would inject the same block
    /// on every following turn.
    pub async fn take_pending_doc_review(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Option<String>> {
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            let current: Option<Option<String>> = sessions::table
                .find(&session_id)
                .select(sessions::pending_doc_review)
                .first(conn)
                .optional()?;
            let Some(Some(review_id)) = current else {
                return Ok(None);
            };
            diesel::update(sessions::table.find(&session_id))
                .set(sessions::pending_doc_review.eq::<Option<String>>(None))
                .execute(conn)?;
            Ok(Some(review_id))
        })
        .await
    }
}

/// Move every annotation on `review_id` from the lines it held in `old` to
/// the lines that hold the same content in `new`, and restamp it with the
/// version it now addresses.
///
/// Resolved annotations are remapped too: their fainter mark in the
/// document pane is how a reader traces what a pass changed, which is only
/// true while it still sits on the passage it was about.
fn remap_comments(
    conn: &mut SqliteConnection,
    review_id: &str,
    version: i32,
    old: &str,
    new: &str,
) -> anyhow::Result<()> {
    let map = line_map(old, new);
    let comments: Vec<DocReviewComment> = doc_review_comments::table
        .filter(doc_review_comments::review_id.eq(review_id))
        .select(DocReviewComment::as_select())
        .load(conn)?;
    for c in comments {
        let (start, end) = map.remap(c.start_line, c.end_line, c.quote.as_deref());
        if start == c.start_line && end == c.end_line && c.version == version {
            continue;
        }
        diesel::update(doc_review_comments::table.find(&c.id))
            .set((
                doc_review_comments::version.eq(version),
                doc_review_comments::start_line.eq(start),
                doc_review_comments::end_line.eq(end),
            ))
            .execute(conn)?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use diesel::prelude::*;

    use crate::db::Db;
    use crate::db::models::{NewFolder, NewSession, NewUser};

    fn seed_folder(db: &Db) {
        let ts = chrono::Utc::now().to_rfc3339();
        db.with_conn_blocking(move |conn| {
            use crate::db::schema::folders;
            diesel::insert_into(folders::table)
                .values(&NewFolder {
                    id: "f1".into(),
                    name: "F".into(),
                    path: "/tmp/f".into(),
                    created_at: ts,
                })
                .execute(conn)?;
            Ok(())
        })
        .unwrap();
    }

    fn seed_user(db: &Db) {
        let ts = chrono::Utc::now().to_rfc3339();
        db.with_conn_blocking(move |conn| {
            use crate::db::schema::users;
            diesel::insert_into(users::table)
                .values(&NewUser {
                    id: "u1".into(),
                    username: "u".into(),
                    email: None,
                    password_hash: "x".into(),
                    role: "admin".into(),
                    created_at: ts.clone(),
                    updated_at: ts,
                })
                .execute(conn)?;
            Ok(())
        })
        .unwrap();
    }

    async fn seed_review(db: &Db) -> crate::db::models::DocReview {
        seed_folder(db);
        db.create_doc_review(
            "Spec",
            "file",
            "f1:docs/spec.md",
            Some("f1"),
            None,
            "# Spec\nline two\n",
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn create_then_revise_bumps_head_and_keeps_history() {
        let db = Db::in_memory().unwrap();
        let review = seed_review(&db).await;
        assert_eq!(review.current_version, 1);
        assert_eq!(review.status, "annotating");
        assert!(review.session_id.is_none());

        let v2 = db
            .insert_doc_review_version(&review.id, "# Spec v2\n", "tightened intro", "assistant")
            .await
            .unwrap();
        assert_eq!(v2, 2);
        let head = db.get_doc_review(&review.id).await.unwrap().unwrap();
        assert_eq!(head.current_version, 2);

        // Version 1 is still readable in full — revisions never overwrite.
        let first = db
            .get_doc_review_version(&review.id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.markdown, "# Spec\nline two\n");
        assert_eq!(first.created_by, "user");

        let metas = db.list_doc_review_versions(&review.id).await.unwrap();
        assert_eq!(metas.len(), 2);
        assert_eq!(metas[0].version, 2, "newest first");
        assert_eq!(metas[0].note, "tightened intro");
        // The meta list carries no markdown at all — the type has no such
        // field, so a body can never leak into the history payload.
        let json = serde_json::to_value(&metas[0]).unwrap();
        assert!(json.get("markdown").is_none());
    }

    #[tokio::test]
    async fn comment_lifecycle_pending_sent_resolved() {
        let db = Db::in_memory().unwrap();
        let review = seed_review(&db).await;

        let c1 = db
            .add_doc_review_comment(&review.id, 1, (2, 2), Some("line two"), "wrong", "not true")
            .await
            .unwrap();
        let c2 = db
            .add_doc_review_comment(&review.id, 1, (1, 1), None, "expand", "say more")
            .await
            .unwrap();
        assert_eq!(c1.status, "pending");

        let sent = db.mark_pending_comments_sent(&review.id).await.unwrap();
        assert_eq!(sent, 2);
        let open = db.list_doc_review_comments(&review.id, true).await.unwrap();
        assert_eq!(open.len(), 2);
        assert!(open.iter().all(|c| c.status == "sent"));
        assert_eq!(open[0].id, c2.id, "ordered by start_line");

        let applied = db
            .apply_comment_resolutions(
                &review.id,
                vec![
                    (c1.id.clone(), "fixed".into(), Some("rewrote it".into())),
                    (c2.id.clone(), "declined".into(), None),
                ],
            )
            .await
            .unwrap();
        assert_eq!(applied, 2);

        // Resolved comments drop out of the open set but stay on the review.
        assert!(
            db.list_doc_review_comments(&review.id, true)
                .await
                .unwrap()
                .is_empty()
        );
        let all = db
            .list_doc_review_comments(&review.id, false)
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
        let fixed = all.iter().find(|c| c.id == c1.id).unwrap();
        assert_eq!(fixed.status, "fixed");
        assert_eq!(fixed.resolution_note.as_deref(), Some("rewrote it"));

        // A later pass can answer an edited comment.
        let patched = db
            .update_doc_review_comment(&c2.id, Some("say much more"), None, Some("answered"), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(patched.body, "say much more");
        assert_eq!(patched.status, "answered");
        assert_eq!(patched.kind, "expand", "untouched fields survive");

        assert_eq!(db.delete_doc_review_comment(&c2.id).await.unwrap(), 1);
        assert_eq!(
            db.list_doc_review_comments(&review.id, false)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn apply_comment_resolutions_rejects_unknown_ids_and_actions() {
        let db = Db::in_memory().unwrap();
        let review = seed_review(&db).await;
        let c1 = db
            .add_doc_review_comment(&review.id, 1, (1, 1), None, "comment", "hm")
            .await
            .unwrap();

        let err = db
            .apply_comment_resolutions(&review.id, vec![("nope".into(), "fixed".into(), None)])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown comment_id"), "{err}");
        assert!(err.contains(&c1.id), "error lists the valid ids: {err}");

        let err = db
            .apply_comment_resolutions(&review.id, vec![(c1.id.clone(), "maybe".into(), None)])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid resolution action"), "{err}");

        // Nothing was written by either rejected batch.
        let after = db
            .list_doc_review_comments(&review.id, false)
            .await
            .unwrap();
        assert_eq!(after[0].status, "pending");
    }

    #[tokio::test]
    async fn delete_review_takes_versions_comments_and_tabs_with_it() {
        let db = Db::in_memory().unwrap();
        seed_user(&db);
        let review = seed_review(&db).await;
        db.insert_doc_review_version(&review.id, "v2", "n", "assistant")
            .await
            .unwrap();
        db.add_doc_review_comment(&review.id, 1, (1, 1), None, "comment", "hm")
            .await
            .unwrap();
        assert!(
            db.upsert_user_tab("u1", "doc_review", &review.id)
                .await
                .unwrap()
                .is_some(),
            "doc_review is an accepted tab kind"
        );

        assert_eq!(db.delete_doc_review(&review.id).await.unwrap(), 1);
        assert!(db.get_doc_review(&review.id).await.unwrap().is_none());
        assert!(
            db.list_doc_review_versions(&review.id)
                .await
                .unwrap()
                .is_empty(),
            "versions cascade with the review"
        );
        assert!(
            db.list_doc_review_comments(&review.id, false)
                .await
                .unwrap()
                .is_empty(),
            "comments cascade with the review"
        );
        assert!(db.list_user_tabs("u1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn pending_doc_review_flag_is_one_shot() {
        let db = Db::in_memory().unwrap();
        let review = seed_review(&db).await;
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_session(NewSession {
            id: "s1".into(),
            name: "review".into(),
            folder_id: "f1".into(),
            is_expert: true,
            expert_kind: Some("doc-review".into()),
            created_at: ts.clone(),
            last_activity: ts,
            ..Default::default()
        })
        .await
        .unwrap();

        db.set_doc_review_session(&review.id, Some("s1"))
            .await
            .unwrap();
        assert_eq!(
            db.get_review_for_session("s1").await.unwrap().unwrap().id,
            review.id
        );

        assert!(db.take_pending_doc_review("s1").await.unwrap().is_none());
        db.set_pending_doc_review("s1", &review.id).await.unwrap();
        assert_eq!(
            db.take_pending_doc_review("s1").await.unwrap().as_deref(),
            Some(review.id.as_str())
        );
        assert!(
            db.take_pending_doc_review("s1").await.unwrap().is_none(),
            "the flag is consumed by the first take"
        );

        db.set_doc_review_status(&review.id, "needs_input")
            .await
            .unwrap();
        assert_eq!(
            db.get_doc_review(&review.id).await.unwrap().unwrap().status,
            "needs_input"
        );
        assert_eq!(db.list_doc_reviews().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_revision_moves_every_anchor_onto_the_content_it_was_about() {
        let db = Db::in_memory().unwrap();
        seed_folder(&db);
        let review = db
            .create_doc_review(
                "Spec",
                "file",
                "f1:docs/spec.md",
                Some("f1"),
                None,
                "# Spec\n\nAlpha paragraph.\n\nBeta paragraph.\n",
            )
            .await
            .unwrap();
        let alpha = db
            .add_doc_review_comment(
                &review.id,
                1,
                (3, 3),
                Some("Alpha paragraph."),
                "comment",
                "tighten this",
            )
            .await
            .unwrap();
        let beta = db
            .add_doc_review_comment(
                &review.id,
                1,
                (5, 5),
                Some("Beta paragraph."),
                "wrong",
                "that number is off",
            )
            .await
            .unwrap();
        // A resolved annotation is remapped too: its fainter mark is how a
        // reader traces what the pass changed.
        db.apply_comment_resolutions(&review.id, vec![(beta.id.clone(), "fixed".into(), None)])
            .await
            .unwrap();

        // Two lines land above both anchors.
        db.insert_doc_review_version(
            &review.id,
            "# Spec\n\nA new opening line.\n\nAlpha paragraph.\n\nBeta paragraph.\n",
            "added an opener",
            "assistant",
        )
        .await
        .unwrap();

        let comments = db
            .list_doc_review_comments(&review.id, false)
            .await
            .unwrap();
        let at = |id: &str| {
            let c = comments.iter().find(|c| c.id == id).unwrap();
            (c.version, c.start_line, c.end_line)
        };
        assert_eq!(
            at(&alpha.id),
            (2, 5, 5),
            "open annotation follows its lines"
        );
        assert_eq!(at(&beta.id), (2, 7, 7), "resolved annotation follows too");
    }

    #[tokio::test]
    async fn an_imported_annotation_is_recognised_on_the_next_sync() {
        let db = Db::in_memory().unwrap();
        let review = seed_review(&db).await;
        let import = |line: i32, body: &'static str, external: &'static str| {
            let id = review.id.clone();
            let db = &db;
            async move {
                db.import_doc_review_comment(
                    &id,
                    1,
                    (line, line),
                    None,
                    "comment",
                    body,
                    "github_pr",
                    external,
                )
                .await
                .unwrap()
            }
        };

        let (first, created) = import(2, "@octocat on GitHub: tighten this", "4242").await;
        assert!(created, "the first sync imports it");
        assert_eq!(first.external_kind.as_deref(), Some("github_pr"));

        // The same GitHub comment on a later sync is the same annotation,
        // not a second copy of the thread.
        let (again, created) = import(2, "@octocat on GitHub: tighten this", "4242").await;
        assert!(!created);
        assert_eq!(again.id, first.id);

        // A different comment still imports.
        let (_, created) = import(3, "@hubot on GitHub: and this", "4243").await;
        assert!(created);
        assert_eq!(
            db.list_doc_review_comments(&review.id, false)
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn a_pull_request_link_is_one_per_review_and_clearable() {
        let db = Db::in_memory().unwrap();
        let review = seed_review(&db).await;
        assert!(
            db.get_doc_review_pr_link(&review.id)
                .await
                .unwrap()
                .is_none()
        );

        db.set_doc_review_pr_link(&review.id, "acme", "app", 7, "docs/spec.md")
            .await
            .unwrap();
        // Re-linking points the review somewhere else rather than stacking a
        // second link on one document.
        let link = db
            .set_doc_review_pr_link(&review.id, "acme", "app", 9, "docs/spec.md")
            .await
            .unwrap();
        assert_eq!(link.number, 9);
        assert!(link.last_synced_at.is_none());

        db.touch_doc_review_pr_sync(&review.id).await.unwrap();
        let synced = db
            .get_doc_review_pr_link(&review.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(synced.number, 9);
        assert!(synced.last_synced_at.is_some());

        assert_eq!(db.clear_doc_review_pr_link(&review.id).await.unwrap(), 1);
        assert!(
            db.get_doc_review_pr_link(&review.id)
                .await
                .unwrap()
                .is_none()
        );
    }
}
