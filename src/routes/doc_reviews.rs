//! `/api/doc-reviews/*` — AI-assisted review passes over a markdown document.
//!
//! A review owns an append-only stack of versions of one document plus the
//! annotations a human anchors to its lines. Nothing here talks to an agent:
//! this surface creates reviews from a source ([`crate::service::doc_review_sources`]),
//! serves the current document + open comments, records annotations, and
//! pushes an approved version back to the source. Running a pass (which wakes
//! the review session) is `POST /api/doc-reviews/{id}/pass`, added by the AI
//! slice.
//!
//! Two invariants shape the endpoints:
//!
//! - **History is append-only.** `revert/{n}` does not delete versions
//!   `n+1..=head`; it copies version `n` to a *new* head. Every state the
//!   document has been in stays diffable.
//! - **The source is only ever written by `apply`.** Revisions land in
//!   `doc_review_versions`, so a user can run unlimited passes and still walk
//!   away without touching the file on disk.

use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{get, post},
};
use std::sync::Arc;

use crate::auth::middleware::{AuthUser, require_auth};
use crate::db::models::{DocReview, DocReviewComment, NewSession};
use crate::service::doc_review_sources as sources;
use crate::service::doc_reviews as review_service;
use crate::service::mcp_server::ExpertDispatcher;
use crate::state::AppState;
use crate::ws::broadcaster::WsEvent;

const MAX_COMMENT_LEN: usize = 10_000;
const MAX_TITLE_LEN: usize = 300;

/// Annotation kinds the doc pane's selection popover can produce.
const COMMENT_KINDS: [&str; 5] = ["comment", "suggest", "wrong", "expand", "shorten"];
/// Lifecycle of one annotation: queued, handed to the assistant, resolved.
const COMMENT_STATUSES: [&str; 5] = ["pending", "sent", "fixed", "declined", "answered"];

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/doc-reviews", get(list_reviews).post(create_review))
        .route(
            "/api/doc-reviews/{id}",
            get(get_review).delete(delete_review),
        )
        .route("/api/doc-reviews/{id}/versions", get(list_versions))
        .route("/api/doc-reviews/{id}/versions/{n}", get(get_version))
        .route("/api/doc-reviews/{id}/comments", post(add_comment))
        .route(
            "/api/doc-reviews/{id}/comments/{comment_id}",
            axum::routing::patch(update_comment).delete(delete_comment),
        )
        .route("/api/doc-reviews/{id}/pass", post(run_pass))
        .route("/api/doc-reviews/{id}/apply", post(apply))
        .route("/api/doc-reviews/{id}/revert/{n}", post(revert))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

fn err(status: StatusCode, msg: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    (
        status,
        Json(serde_json::json!({ "error": msg.to_string() })),
    )
}

/// Tell every connected client the review moved. Keyed by the review id
/// (not a session id) so the review screen can subscribe to it the same way
/// a session subscribes to its own stream.
fn broadcast_update(state: &AppState, review: &DocReview) {
    review_service::broadcast_update(&state.broadcaster, review);
}

/// Re-read the review and broadcast its (possibly changed) head/status.
async fn broadcast_by_id(state: &AppState, id: &str) {
    review_service::broadcast_by_id(&state.db, &state.broadcaster, id).await;
}

/// GET /api/doc-reviews → every review, most recently touched first.
async fn list_reviews(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.db.list_doc_reviews().await {
        Ok(reviews) => Ok(Json(serde_json::json!({ "reviews": reviews }))),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

#[derive(serde::Deserialize)]
struct CreateReviewBody {
    source_kind: String,
    source_ref: String,
    /// Override the title resolved from the document.
    title: Option<String>,
    project_id: Option<String>,
    /// Defaults to the folder in a `file` source_ref.
    folder_id: Option<String>,
}

/// POST /api/doc-reviews → snapshot the source as version 1 of a new review.
///
/// The source is read *before* the row is written, so a review never exists
/// pointing at something unreadable — the wizard gets the reason back instead.
async fn create_review(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateReviewBody>,
) -> impl IntoResponse {
    if !sources::SOURCE_KINDS.contains(&body.source_kind.as_str()) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!(
                "source_kind must be one of: {}",
                sources::SOURCE_KINDS.join(", ")
            ),
        ));
    }
    if body.source_ref.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "source_ref is required"));
    }
    let doc = match sources::load(&state, &body.source_kind, &body.source_ref).await {
        Ok(d) => d,
        Err(e) => return Err(err(StatusCode::BAD_REQUEST, e)),
    };
    let title = match body.title.as_deref().map(str::trim) {
        Some(t) if !t.is_empty() => t.chars().take(MAX_TITLE_LEN).collect::<String>(),
        _ => doc.title.chars().take(MAX_TITLE_LEN).collect::<String>(),
    };
    let folder_id = body
        .folder_id
        .clone()
        .or_else(|| sources::folder_id_for(&body.source_kind, &body.source_ref));

    match state
        .db
        .create_doc_review(
            &title,
            &body.source_kind,
            &body.source_ref,
            folder_id.as_deref(),
            body.project_id.as_deref(),
            &doc.markdown,
        )
        .await
    {
        Ok(review) => {
            broadcast_update(&state, &review);
            Ok((
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "review": review,
                    "markdown": doc.markdown,
                    "comments": Vec::<serde_json::Value>::new(),
                })),
            ))
        }
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

/// GET /api/doc-reviews/{id} → the review, its current markdown, and the
/// annotations still awaiting the assistant. One request is everything the
/// review screen needs to render.
async fn get_review(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let review = match state.db.get_doc_review(&id).await {
        Ok(Some(r)) => r,
        Ok(None) => return Err(err(StatusCode::NOT_FOUND, "review not found")),
        Err(e) => return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    };
    let markdown = state
        .db
        .get_doc_review_version(&id, review.current_version)
        .await
        .ok()
        .flatten()
        .map(|v| v.markdown)
        .unwrap_or_default();
    let comments = state
        .db
        .list_doc_review_comments(&id, true)
        .await
        .unwrap_or_default();
    Ok(Json(serde_json::json!({
        "review": review,
        "markdown": markdown,
        "comments": comments,
    })))
}

/// DELETE /api/doc-reviews/{id} → drop the review, its versions, comments,
/// and any tab pinned to it. The source document is untouched.
async fn delete_review(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let review = match state.db.get_doc_review(&id).await {
        Ok(Some(r)) => r,
        Ok(None) => return Err(err(StatusCode::NOT_FOUND, "review not found")),
        Err(e) => return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    };
    match state.db.delete_doc_review(&id).await {
        Ok(_) => {
            broadcast_update(&state, &review);
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

/// GET /api/doc-reviews/{id}/versions → history metadata, newest first
/// (without the markdown bodies).
async fn list_versions(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.db.list_doc_review_versions(&id).await {
        Ok(versions) => Ok(Json(serde_json::json!({ "versions": versions }))),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

/// GET /api/doc-reviews/{id}/versions/{n} → one version with its markdown,
/// for the diff view.
async fn get_version(
    State(state): State<Arc<AppState>>,
    Path((id, n)): Path<(String, i32)>,
) -> impl IntoResponse {
    match state.db.get_doc_review_version(&id, n).await {
        Ok(Some(v)) => Ok(Json(serde_json::json!({ "version": v }))),
        Ok(None) => Err(err(StatusCode::NOT_FOUND, "version not found")),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

#[derive(serde::Deserialize)]
struct AddCommentBody {
    start_line: i32,
    end_line: Option<i32>,
    quote: Option<String>,
    kind: String,
    body: String,
}

/// POST /api/doc-reviews/{id}/comments → anchor an annotation to a line range
/// of the current version. Allowed in any status: annotations queued while a
/// pass is running stay `pending` and ride along on the next one.
async fn add_comment(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<AddCommentBody>,
) -> impl IntoResponse {
    let review = match state.db.get_doc_review(&id).await {
        Ok(Some(r)) => r,
        Ok(None) => return Err(err(StatusCode::NOT_FOUND, "review not found")),
        Err(e) => return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    };
    if !COMMENT_KINDS.contains(&req.kind.as_str()) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("kind must be one of: {}", COMMENT_KINDS.join(", ")),
        ));
    }
    let body = req.body.trim();
    if body.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "comment body is empty"));
    }
    if body.len() > MAX_COMMENT_LEN {
        return Err(err(StatusCode::BAD_REQUEST, "comment too long"));
    }
    let start_line = req.start_line.max(1);
    let end_line = req.end_line.unwrap_or(start_line).max(start_line);
    let quote = req.quote.as_deref().map(|q| {
        // The quote is a display aid after a revision moves the lines, not the
        // anchor itself — a long selection doesn't need to be stored whole.
        q.chars().take(MAX_COMMENT_LEN).collect::<String>()
    });

    match state
        .db
        .add_doc_review_comment(
            &id,
            review.current_version,
            (start_line, end_line),
            quote.as_deref(),
            &req.kind,
            body,
        )
        .await
    {
        Ok(c) => {
            broadcast_update(&state, &review);
            Ok((
                StatusCode::CREATED,
                Json(serde_json::json!({ "comment": c })),
            ))
        }
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

#[derive(serde::Deserialize)]
struct UpdateCommentBody {
    body: Option<String>,
    kind: Option<String>,
    status: Option<String>,
    resolution_note: Option<String>,
}

/// PATCH /api/doc-reviews/{id}/comments/{comment_id} → edit an annotation
/// (fix a typo before a pass, or resolve it by hand).
async fn update_comment(
    State(state): State<Arc<AppState>>,
    Path((id, comment_id)): Path<(String, String)>,
    Json(req): Json<UpdateCommentBody>,
) -> impl IntoResponse {
    if let Some(kind) = req.kind.as_deref()
        && !COMMENT_KINDS.contains(&kind)
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("kind must be one of: {}", COMMENT_KINDS.join(", ")),
        ));
    }
    if let Some(status) = req.status.as_deref()
        && !COMMENT_STATUSES.contains(&status)
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("status must be one of: {}", COMMENT_STATUSES.join(", ")),
        ));
    }
    let body = req.body.as_deref().map(str::trim);
    if let Some(b) = body {
        if b.is_empty() {
            return Err(err(StatusCode::BAD_REQUEST, "comment body is empty"));
        }
        if b.len() > MAX_COMMENT_LEN {
            return Err(err(StatusCode::BAD_REQUEST, "comment too long"));
        }
    }

    match state
        .db
        .update_doc_review_comment(
            &comment_id,
            body,
            req.kind.as_deref(),
            req.status.as_deref(),
            req.resolution_note.as_deref(),
        )
        .await
    {
        Ok(Some(c)) => {
            broadcast_by_id(&state, &id).await;
            Ok(Json(serde_json::json!({ "comment": c })))
        }
        Ok(None) => Err(err(StatusCode::NOT_FOUND, "comment not found")),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

/// DELETE /api/doc-reviews/{id}/comments/{comment_id}
async fn delete_comment(
    State(state): State<Arc<AppState>>,
    Path((id, comment_id)): Path<(String, String)>,
) -> impl IntoResponse {
    match state.db.delete_doc_review_comment(&comment_id).await {
        Ok(0) => Err(err(StatusCode::NOT_FOUND, "comment not found")),
        Ok(_) => {
            broadcast_by_id(&state, &id).await;
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

#[derive(serde::Deserialize)]
#[serde(default)]
struct PassBody {
    /// Free-form ask: the chat lane's message, or extra context alongside
    /// the queued annotations.
    message: Option<String>,
    /// Hand the queued annotations to the assistant. The chat lane and the
    /// inline Clarify action send `false` — they are conversation about the
    /// document, not a revision pass over the annotations.
    include_annotations: bool,
}

impl Default for PassBody {
    fn default() -> Self {
        Self {
            message: None,
            include_annotations: true,
        }
    }
}

/// POST /api/doc-reviews/{id}/pass — wake the review session on this
/// document.
///
/// The session is created on the FIRST pass and reused forever after, so a
/// review is one continuous conversation about one document. Three things
/// have to be true before the agent runs: the queued annotations are marked
/// `sent` (new ones added mid-run stay `pending` and ride the next pass),
/// the review reads `running`, and `sessions.pending_doc_review` is armed so
/// the turn opens with the document itself (see
/// [`crate::handover::take_pending_injection`]).
async fn run_pass(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let review = match state.db.get_doc_review(&id).await {
        Ok(Some(r)) => r,
        Ok(None) => return Err(err(StatusCode::NOT_FOUND, "review not found")),
        Err(e) => return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    };
    // "Run the pass I already queued" is a body-less POST, so an empty body
    // means the defaults rather than a malformed request.
    let body: PassBody = if body.is_empty() {
        PassBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(b) => b,
            Err(e) => return Err(err(StatusCode::BAD_REQUEST, e)),
        }
    };
    let (message, include_annotations) = (body.message, body.include_annotations);
    let message: Option<String> = message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(|m| m.chars().take(MAX_COMMENT_LEN).collect());

    // Only the annotations queued since the last pass are "new work";
    // already-`sent` ones ride along in the injection either way.
    let pending: Vec<DocReviewComment> = if include_annotations {
        match state.db.list_doc_review_comments(&id, true).await {
            Ok(all) => all.into_iter().filter(|c| c.status == "pending").collect(),
            Err(e) => return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
        }
    } else {
        Vec::new()
    };
    if pending.is_empty() && message.is_none() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "nothing to run: annotate the document or send a message first",
        ));
    }

    let session_id = match ensure_review_session(&state, &review, user.user_id.as_str()).await {
        Ok(s) => s,
        Err(e) => return Err(e),
    };

    if include_annotations && let Err(e) = state.db.mark_pending_comments_sent(&id).await {
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e));
    }
    if let Err(e) = state.db.set_doc_review_status(&id, "running").await {
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e));
    }
    if let Err(e) = state.db.set_pending_doc_review(&session_id, &id).await {
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e));
    }

    let instruction = pass_instruction(&pending, message.as_deref());
    // Persist the turn ourselves — `resume_session` only drives the agent,
    // and the chat lane renders the session's own event log.
    match state
        .db
        .append_event(
            &session_id,
            "user",
            serde_json::json!({ "text": &instruction, "source": "doc-review-pass" }),
        )
        .await
    {
        Ok(ev) => state.broadcaster.broadcast(WsEvent {
            event_type: "event".into(),
            session_id: session_id.clone(),
            data: serde_json::json!({
                "id": ev.id,
                "seq": ev.seq,
                "ts": ev.ts,
                "kind": "user",
                "data": { "text": &instruction, "source": "doc-review-pass" },
            }),
        }),
        Err(e) => tracing::warn!(review_id = %id, "doc review pass: user event failed: {e}"),
    }

    // Same seam every other in-process resume uses: spawn if idle, queue or
    // inject if the session is mid-turn. A dispatch failure (no provider
    // configured, CLI missing) is logged rather than fatal — the pass is
    // already durable, and the user can retry it from the same state.
    if let Err(e) = crate::service::mcp_server::AppExpertDispatcher::new(state.clone())
        .resume_session(&session_id, &instruction)
        .await
    {
        tracing::warn!(review_id = %id, session_id = %session_id, "doc review pass dispatch failed: {e}");
    }

    let updated = state
        .db
        .get_doc_review(&id)
        .await
        .ok()
        .flatten()
        .unwrap_or(review);
    broadcast_update(&state, &updated);
    Ok(Json(serde_json::json!({
        "review": updated,
        "session_id": session_id,
        "annotations_sent": pending.len(),
    })))
}

/// The review's AI session, created on first use. `expert_kind` is what the
/// MCP layer keys the review-only tools off, and the system prompt is the
/// feature's whole behaviour contract — both live in
/// [`crate::service::doc_reviews`].
async fn ensure_review_session(
    state: &Arc<AppState>,
    review: &DocReview,
    user_id: &str,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    // A session id that no longer resolves (the session was deleted) is
    // treated as absent rather than fatal: the next pass gets a fresh one.
    if let Some(existing) = review.session_id.as_deref()
        && state
            .db
            .get_session(existing)
            .await
            .ok()
            .flatten()
            .is_some()
    {
        return Ok(existing.to_string());
    }

    // Every session needs a working directory. Reviews of a report or a
    // plan carry no folder of their own, so they fall back to the first
    // workspace folder.
    let folder_id = match review.folder_id.clone() {
        Some(f) => f,
        None => match state.db.list_folders().await {
            Ok(folders) => match folders.into_iter().next() {
                Some(f) => f.id,
                None => {
                    return Err(err(
                        StatusCode::BAD_REQUEST,
                        "add a workspace folder before running a review pass",
                    ));
                }
            },
            Err(e) => return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
        },
    };

    let now = chrono::Utc::now().to_rfc3339();
    let session = state
        .db
        .create_session(NewSession {
            id: uuid::Uuid::new_v4().to_string(),
            name: format!("Review: {}", review.title)
                .chars()
                .take(MAX_TITLE_LEN)
                .collect(),
            folder_id,
            is_expert: true,
            expert_kind: Some(review_service::EXPERT_KIND.to_string()),
            project_id: review.project_id.clone(),
            system_prompt: Some(review_service::SYSTEM_PROMPT.to_string()),
            user_id: Some(user_id.to_string()),
            created_at: now.clone(),
            last_activity: now,
            ..Default::default()
        })
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    state
        .db
        .set_doc_review_session(&review.id, Some(&session.id))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(session.id)
}

/// The turn text for a pass. The document and the annotations' full text
/// already arrive via the injection, so this is the *instruction*: what to
/// do with them, plus a digest so the ask survives a long turn.
fn pass_instruction(pending: &[DocReviewComment], message: Option<&str>) -> String {
    let mut out = String::new();
    if !pending.is_empty() {
        out.push_str(&format!(
            "Review pass: {} new annotation(s) on the document above.\n\n",
            pending.len()
        ));
        for c in pending {
            out.push_str(&format!(
                "- [lines {}-{}] ({}) {} (id: {})\n",
                c.start_line,
                c.end_line,
                c.kind,
                c.body.trim(),
                c.id
            ));
        }
        out.push_str(
            "\nAddress every open annotation, then submit the FULL revised document with \
             `submit_review_revision`, reporting each annotation in `resolutions`. Ask with \
             `ask_user` instead of guessing when an intent is unclear.",
        );
    }
    if let Some(message) = message {
        if !out.is_empty() {
            out.push_str("\n\nThe user also says:\n");
        }
        out.push_str(message);
    }
    out
}

#[derive(serde::Deserialize)]
struct ApplyBody {
    /// Also mark the review approved — the user is done with this document.
    #[serde(default)]
    finish: bool,
}

/// POST /api/doc-reviews/{id}/apply → write the current version back to the
/// source document. This is the only endpoint that touches the original.
async fn apply(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<ApplyBody>>,
) -> impl IntoResponse {
    let review = match state.db.get_doc_review(&id).await {
        Ok(Some(r)) => r,
        Ok(None) => return Err(err(StatusCode::NOT_FOUND, "review not found")),
        Err(e) => return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    };
    // Body-less apply is allowed: "apply, don't finish" is the common case.
    let finish = body.map(|Json(b)| b.finish).unwrap_or(false);
    let markdown = match state
        .db
        .get_doc_review_version(&id, review.current_version)
        .await
    {
        Ok(Some(v)) => v.markdown,
        Ok(None) => return Err(err(StatusCode::NOT_FOUND, "current version not found")),
        Err(e) => return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    };
    if let Err(e) = sources::write(&state, &review.source_kind, &review.source_ref, &markdown).await
    {
        return Err(err(StatusCode::BAD_REQUEST, e));
    }
    if finish && let Err(e) = state.db.set_doc_review_status(&id, "approved").await {
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e));
    }
    let updated = state
        .db
        .get_doc_review(&id)
        .await
        .ok()
        .flatten()
        .unwrap_or(review);
    broadcast_update(&state, &updated);
    Ok(Json(serde_json::json!({ "review": updated })))
}

/// POST /api/doc-reviews/{id}/revert/{n} → copy version `n` to a new head.
///
/// Appending rather than truncating keeps every revision diffable: reverting
/// is itself a version the user can revert.
async fn revert(
    State(state): State<Arc<AppState>>,
    Path((id, n)): Path<(String, i32)>,
) -> impl IntoResponse {
    let target = match state.db.get_doc_review_version(&id, n).await {
        Ok(Some(v)) => v,
        Ok(None) => return Err(err(StatusCode::NOT_FOUND, "version not found")),
        Err(e) => return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    };
    let version = match state
        .db
        .insert_doc_review_version(&id, &target.markdown, &format!("revert to v{n}"), "user")
        .await
    {
        Ok(v) => v,
        Err(e) => return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    };
    let review = match state.db.get_doc_review(&id).await {
        Ok(Some(r)) => r,
        Ok(None) => return Err(err(StatusCode::NOT_FOUND, "review not found")),
        Err(e) => return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    };
    broadcast_update(&state, &review);
    Ok(Json(serde_json::json!({
        "review": review,
        "version": version,
        "markdown": target.markdown,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::middleware::tests::{seed_authenticated_user, test_state};
    use crate::db::models::NewFolder;
    use axum::body::Body;
    use axum::http::{Request, header};
    use tower::ServiceExt;

    fn app(state: Arc<AppState>) -> Router {
        Router::new().merge(router(state.clone())).with_state(state)
    }

    fn req(method: &str, uri: &str, token: &str, body: Option<serde_json::Value>) -> Request<Body> {
        let builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json");
        match body {
            Some(b) => builder.body(Body::from(b.to_string())).unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        }
    }

    async fn json_body(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }

    /// A state whose data dir is `dir`, with a workspace folder `f1` rooted at
    /// `workspace` holding `doc.md`.
    async fn fixture(
        dir: &std::path::Path,
        workspace: &std::path::Path,
    ) -> (Arc<AppState>, String) {
        let state = test_state(dir);
        let token = seed_authenticated_user(&state, "admin").await;
        std::fs::create_dir_all(workspace.join("docs")).unwrap();
        std::fs::write(
            workspace.join("docs/doc.md"),
            "# Doc Title\n\nline two\nline three\n",
        )
        .unwrap();
        state
            .db
            .create_folder(NewFolder {
                id: "f1".into(),
                name: "Repo".into(),
                path: workspace.to_string_lossy().to_string(),
                created_at: chrono::Utc::now().to_rfc3339(),
            })
            .await
            .unwrap();
        (state, token)
    }

    #[tokio::test]
    async fn file_review_crud_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (state, token) = fixture(dir.path(), workspace.path()).await;

        // Create: title comes from the document's first heading.
        let response = app(state.clone())
            .oneshot(req(
                "POST",
                "/api/doc-reviews",
                &token,
                Some(serde_json::json!({
                    "source_kind": "file",
                    "source_ref": "f1:docs/doc.md",
                })),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let created = json_body(response).await;
        let id = created["review"]["id"].as_str().unwrap().to_string();
        assert_eq!(created["review"]["title"], "Doc Title");
        assert_eq!(created["review"]["folder_id"], "f1");
        assert_eq!(created["review"]["status"], "annotating");
        assert_eq!(created["review"]["current_version"], 1);

        // Get: review + current markdown + open comments in one payload.
        let response = app(state.clone())
            .oneshot(req("GET", &format!("/api/doc-reviews/{id}"), &token, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let detail = json_body(response).await;
        assert!(
            detail["markdown"].as_str().unwrap().contains("line two"),
            "markdown served: {detail}"
        );
        assert_eq!(detail["comments"].as_array().unwrap().len(), 0);

        // Annotate, then confirm the comment shows up as open on the detail.
        let response = app(state.clone())
            .oneshot(req(
                "POST",
                &format!("/api/doc-reviews/{id}/comments"),
                &token,
                Some(serde_json::json!({
                    "start_line": 3,
                    "end_line": 4,
                    "quote": "line two",
                    "kind": "wrong",
                    "body": "this is wrong",
                })),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let comment_id = json_body(response).await["comment"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        let detail = json_body(
            app(state.clone())
                .oneshot(req("GET", &format!("/api/doc-reviews/{id}"), &token, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(detail["comments"].as_array().unwrap().len(), 1);
        assert_eq!(detail["comments"][0]["status"], "pending");

        // An unknown kind is refused rather than stored.
        let response = app(state.clone())
            .oneshot(req(
                "POST",
                &format!("/api/doc-reviews/{id}/comments"),
                &token,
                Some(serde_json::json!({
                    "start_line": 1,
                    "kind": "rewrite-everything",
                    "body": "nope",
                })),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // Patch + delete the annotation.
        let response = app(state.clone())
            .oneshot(req(
                "PATCH",
                &format!("/api/doc-reviews/{id}/comments/{comment_id}"),
                &token,
                Some(serde_json::json!({ "body": "actually fine", "status": "declined" })),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let patched = json_body(response).await;
        assert_eq!(patched["comment"]["body"], "actually fine");
        assert_eq!(patched["comment"]["status"], "declined");

        let response = app(state.clone())
            .oneshot(req(
                "DELETE",
                &format!("/api/doc-reviews/{id}/comments/{comment_id}"),
                &token,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        // Delete the review itself.
        let response = app(state.clone())
            .oneshot(req(
                "DELETE",
                &format!("/api/doc-reviews/{id}"),
                &token,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(state.db.list_doc_reviews().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn every_route_requires_auth() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (state, _token) = fixture(dir.path(), workspace.path()).await;

        for (method, uri) in [
            ("GET", "/api/doc-reviews"),
            ("POST", "/api/doc-reviews"),
            ("GET", "/api/doc-reviews/r1"),
            ("DELETE", "/api/doc-reviews/r1"),
            ("POST", "/api/doc-reviews/r1/comments"),
            ("PATCH", "/api/doc-reviews/r1/comments/c1"),
            ("DELETE", "/api/doc-reviews/r1/comments/c1"),
            ("POST", "/api/doc-reviews/r1/apply"),
            ("POST", "/api/doc-reviews/r1/revert/1"),
        ] {
            let response = app(state.clone())
                .oneshot(req(method, uri, "not-a-token", Some(serde_json::json!({}))))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {uri} must require auth"
            );
        }
    }

    #[tokio::test]
    async fn create_refuses_unreadable_and_non_markdown_sources() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (state, token) = fixture(dir.path(), workspace.path()).await;
        std::fs::write(workspace.path().join("notes.txt"), "plain").unwrap();
        // A file outside the folder that a traversal would reach.
        std::fs::write(
            workspace.path().parent().unwrap().join("outside_doc.md"),
            "# Outside\n",
        )
        .unwrap();

        for (kind, source_ref, why) in [
            ("file", "f1:../outside_doc.md", "traversal"),
            ("file", "f1:/etc/hosts", "absolute path"),
            ("file", "f1:notes.txt", "non-markdown"),
            ("file", "f1:docs/missing.md", "missing file"),
            ("file", "nope:docs/doc.md", "unknown folder"),
            ("report", "2026-07-28/../../secret.md", "report traversal"),
            ("plan", "no-such-plan", "missing plan"),
            ("wat", "f1:docs/doc.md", "unknown kind"),
        ] {
            let response = app(state.clone())
                .oneshot(req(
                    "POST",
                    "/api/doc-reviews",
                    &token,
                    Some(serde_json::json!({ "source_kind": kind, "source_ref": source_ref })),
                ))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "{why} must be refused"
            );
        }
        assert!(state.db.list_doc_reviews().await.unwrap().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn create_refuses_a_symlink_that_escapes_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (state, token) = fixture(dir.path(), workspace.path()).await;
        let secret = workspace.path().parent().unwrap().join("escape_secret.md");
        std::fs::write(&secret, "# TOP SECRET\n").unwrap();
        std::os::unix::fs::symlink(&secret, workspace.path().join("link.md")).unwrap();

        let response = app(state.clone())
            .oneshot(req(
                "POST",
                "/api/doc-reviews",
                &token,
                Some(serde_json::json!({
                    "source_kind": "file",
                    "source_ref": "f1:link.md",
                })),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = json_body(response).await;
        assert!(
            body["error"].as_str().unwrap().contains("escapes"),
            "symlink escape refused: {body}"
        );
        let _ = std::fs::remove_file(&secret);
    }

    #[tokio::test]
    async fn apply_writes_the_source_and_finish_approves() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (state, token) = fixture(dir.path(), workspace.path()).await;

        let created = json_body(
            app(state.clone())
                .oneshot(req(
                    "POST",
                    "/api/doc-reviews",
                    &token,
                    Some(serde_json::json!({
                        "source_kind": "file",
                        "source_ref": "f1:docs/doc.md",
                    })),
                ))
                .await
                .unwrap(),
        )
        .await;
        let id = created["review"]["id"].as_str().unwrap().to_string();

        // A revision lands as v2 without touching the file on disk.
        state
            .db
            .insert_doc_review_version(&id, "# Doc Title\n\nrevised\n", "pass 1", "assistant")
            .await
            .unwrap();
        assert!(
            std::fs::read_to_string(workspace.path().join("docs/doc.md"))
                .unwrap()
                .contains("line two"),
            "a revision must not write the source"
        );

        let response = app(state.clone())
            .oneshot(req(
                "POST",
                &format!("/api/doc-reviews/{id}/apply"),
                &token,
                Some(serde_json::json!({ "finish": true })),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["review"]["status"], "approved");
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("docs/doc.md")).unwrap(),
            "# Doc Title\n\nrevised\n"
        );
    }

    #[tokio::test]
    async fn revert_appends_a_new_head_version() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (state, token) = fixture(dir.path(), workspace.path()).await;
        let created = json_body(
            app(state.clone())
                .oneshot(req(
                    "POST",
                    "/api/doc-reviews",
                    &token,
                    Some(serde_json::json!({
                        "source_kind": "file",
                        "source_ref": "f1:docs/doc.md",
                    })),
                ))
                .await
                .unwrap(),
        )
        .await;
        let id = created["review"]["id"].as_str().unwrap().to_string();
        state
            .db
            .insert_doc_review_version(&id, "# Doc Title\n\nrevised\n", "pass 1", "assistant")
            .await
            .unwrap();

        let response = app(state.clone())
            .oneshot(req(
                "POST",
                &format!("/api/doc-reviews/{id}/revert/1"),
                &token,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let reverted = json_body(response).await;
        assert_eq!(reverted["version"], 3, "revert appends: {reverted}");
        assert_eq!(reverted["review"]["current_version"], 3);

        // History keeps all three versions, v3 is a copy of v1.
        let versions = json_body(
            app(state.clone())
                .oneshot(req(
                    "GET",
                    &format!("/api/doc-reviews/{id}/versions"),
                    &token,
                    None,
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(versions["versions"].as_array().unwrap().len(), 3);
        assert_eq!(versions["versions"][0]["note"], "revert to v1");
        assert_eq!(versions["versions"][0]["created_by"], "user");

        let v3 = json_body(
            app(state.clone())
                .oneshot(req(
                    "GET",
                    &format!("/api/doc-reviews/{id}/versions/3"),
                    &token,
                    None,
                ))
                .await
                .unwrap(),
        )
        .await;
        assert!(
            v3["version"]["markdown"]
                .as_str()
                .unwrap()
                .contains("line two"),
            "v3 restores v1's text: {v3}"
        );

        // A version that never existed is a clean 404.
        let response = app(state.clone())
            .oneshot(req(
                "GET",
                &format!("/api/doc-reviews/{id}/versions/99"),
                &token,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn report_apply_preserves_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (state, token) = fixture(dir.path(), workspace.path()).await;
        let report_dir = dir.path().join("reports").join("2026-07-28");
        std::fs::create_dir_all(&report_dir).unwrap();
        std::fs::write(
            report_dir.join("audit.md"),
            "---\ntitle: Audit\nsession_id: s9\n---\n\n# Audit\n\noriginal\n",
        )
        .unwrap();

        let created = json_body(
            app(state.clone())
                .oneshot(req(
                    "POST",
                    "/api/doc-reviews",
                    &token,
                    Some(serde_json::json!({
                        "source_kind": "report",
                        "source_ref": "2026-07-28/audit.md",
                    })),
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(created["review"]["title"], "Audit");
        assert!(
            !created["markdown"].as_str().unwrap().contains("session_id"),
            "the reviewed body excludes frontmatter: {created}"
        );
        let id = created["review"]["id"].as_str().unwrap().to_string();

        state
            .db
            .insert_doc_review_version(&id, "# Audit\n\nrevised\n", "pass 1", "assistant")
            .await
            .unwrap();
        let response = app(state.clone())
            .oneshot(req(
                "POST",
                &format!("/api/doc-reviews/{id}/apply"),
                &token,
                Some(serde_json::json!({})),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let raw = std::fs::read_to_string(report_dir.join("audit.md")).unwrap();
        assert!(raw.contains("session_id: s9"), "frontmatter kept: {raw}");
        assert!(raw.contains("revised"), "body applied: {raw}");
        assert!(!raw.contains("original"), "body replaced: {raw}");
        // apply without `finish` leaves the review open for more passes.
        let review = state.db.get_doc_review(&id).await.unwrap().unwrap();
        assert_eq!(review.status, "annotating");
    }

    /// Create a file review over `docs/doc.md` and return its id.
    async fn seed_review(state: &Arc<AppState>, token: &str) -> String {
        let response = app(state.clone())
            .oneshot(req(
                "POST",
                "/api/doc-reviews",
                token,
                Some(serde_json::json!({
                    "source_kind": "file",
                    "source_ref": "f1:docs/doc.md",
                })),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        json_body(response).await["review"]["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    async fn add_annotation(state: &Arc<AppState>, token: &str, id: &str, body: &str) {
        let response = app(state.clone())
            .oneshot(req(
                "POST",
                &format!("/api/doc-reviews/{id}/comments"),
                token,
                Some(serde_json::json!({
                    "start_line": 3,
                    "end_line": 3,
                    "quote": "line two",
                    "kind": "wrong",
                    "body": body,
                })),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    async fn run_pass(
        state: &Arc<AppState>,
        token: &str,
        id: &str,
        body: Option<serde_json::Value>,
    ) -> axum::response::Response {
        app(state.clone())
            .oneshot(req(
                "POST",
                &format!("/api/doc-reviews/{id}/pass"),
                token,
                body,
            ))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_pass_with_nothing_to_say_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (state, token) = fixture(dir.path(), workspace.path()).await;
        let id = seed_review(&state, &token).await;

        let response = run_pass(&state, &token, &id, None).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // Same for the chat lane with an empty message.
        let response = run_pass(
            &state,
            &token,
            &id,
            Some(serde_json::json!({ "include_annotations": false, "message": "   " })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let review = state.db.get_doc_review(&id).await.unwrap().unwrap();
        assert_eq!(
            review.status, "annotating",
            "a refused pass changes nothing"
        );
        assert!(review.session_id.is_none(), "and creates no session");
    }

    #[tokio::test]
    async fn the_first_pass_creates_the_review_session_and_later_passes_reuse_it() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (state, token) = fixture(dir.path(), workspace.path()).await;
        let id = seed_review(&state, &token).await;
        add_annotation(&state, &token, &id, "this is wrong").await;

        let response = run_pass(&state, &token, &id, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["annotations_sent"], 1, "got: {body}");
        let session_id = body["session_id"].as_str().unwrap().to_string();

        let review = state.db.get_doc_review(&id).await.unwrap().unwrap();
        assert_eq!(review.status, "running");
        assert_eq!(review.session_id.as_deref(), Some(session_id.as_str()));

        // The session is a review session, prompted for the job.
        let session = state.db.get_session(&session_id).await.unwrap().unwrap();
        assert!(session.is_expert);
        assert_eq!(
            session.expert_kind.as_deref(),
            Some(review_service::EXPERT_KIND)
        );
        assert_eq!(session.folder_id, "f1", "it inherits the review's folder");
        let prompt = session.system_prompt.clone().unwrap_or_default();
        assert!(
            prompt.contains("submit_review_revision"),
            "the review prompt is installed: {prompt}"
        );

        // The annotation was handed over, and the document is armed for the
        // session's next turn.
        let comments = state.db.list_doc_review_comments(&id, false).await.unwrap();
        assert_eq!(comments[0].status, "sent");
        assert_eq!(
            state
                .db
                .take_pending_doc_review(&session_id)
                .await
                .unwrap()
                .as_deref(),
            Some(id.as_str())
        );

        // The instruction is on the session's log, digest and all.
        let events = state
            .db
            .list_events_by_session(&session_id, None)
            .await
            .unwrap();
        let text = events
            .iter()
            .filter(|e| e.kind == "user")
            .map(|e| e.data.clone())
            .collect::<String>();
        assert!(text.contains("this is wrong"), "got: {text}");
        assert!(text.contains("doc-review-pass"), "got: {text}");

        // A second pass reuses the same session rather than starting a new
        // conversation about the same document.
        add_annotation(&state, &token, &id, "also wrong").await;
        let response = run_pass(
            &state,
            &token,
            &id,
            Some(serde_json::json!({ "message": "and tighten the intro" })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["session_id"], session_id, "same session: {body}");
        assert_eq!(body["annotations_sent"], 1, "only the NEW annotation");
        assert_eq!(
            state
                .db
                .take_pending_doc_review(&session_id)
                .await
                .unwrap()
                .as_deref(),
            Some(id.as_str()),
            "the injection is re-armed for every pass"
        );
    }

    #[tokio::test]
    async fn the_chat_lane_runs_without_consuming_the_queued_annotations() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (state, token) = fixture(dir.path(), workspace.path()).await;
        let id = seed_review(&state, &token).await;
        add_annotation(&state, &token, &id, "this is wrong").await;

        let response = run_pass(
            &state,
            &token,
            &id,
            Some(serde_json::json!({
                "include_annotations": false,
                "message": "why is section 2 here at all?",
            })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["annotations_sent"], 0, "got: {body}");

        let comments = state.db.list_doc_review_comments(&id, false).await.unwrap();
        assert_eq!(
            comments[0].status, "pending",
            "a chat message must not spend the queued annotations"
        );
        let session_id = body["session_id"].as_str().unwrap().to_string();
        let events = state
            .db
            .list_events_by_session(&session_id, None)
            .await
            .unwrap();
        let text = events
            .iter()
            .filter(|e| e.kind == "user")
            .map(|e| e.data.clone())
            .collect::<String>();
        assert!(text.contains("why is section 2 here"), "got: {text}");
        assert!(
            !text.contains("Review pass:"),
            "no annotation digest on a chat turn: {text}"
        );
    }
}
