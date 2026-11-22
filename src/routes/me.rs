//! `/api/me/*` — per-user data not tied to a specific resource.
//!
//! The cross-device tab list: the frontend tab strip
//! fetches `GET /api/me/tabs` on mount and on window focus, POSTs to
//! upsert when a tab is opened (this both creates a new tab and bumps
//! `last_active` so MRU ordering + read state stays in sync across
//! devices), and DELETEs to close. And the user's use cases (`GET`/`PUT
//! /api/me/use-cases`), picked in the setup wizard to order onboarding
//! content.

use axum::body::Body;
use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    http::{Request, StatusCode},
    middleware,
    response::IntoResponse,
    routing::{delete, get},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::auth::middleware::{AuthUser, require_auth};
use crate::state::AppState;

#[derive(Serialize)]
pub struct TabView {
    pub item_type: String,
    pub item_id: String,
    pub last_active: String,
    /// Name of the referenced session/project, denormalized so the tab
    /// strip doesn't have to cross-reference a separate list. This also
    /// lets us include worker sessions (`is_worker=true`) — those are
    /// not surfaced by `GET /api/sessions`, so before we shipped this
    /// the tab strip rendered them as a phantom "Session" chip and the
    /// auto-cleanup loop closed them as soon as the sessions list
    /// loaded.
    pub name: String,
    /// True iff this tab refers to a worker session (a session the
    /// orchestrator spawned for a card). Worker sessions are owned by
    /// their card / project, so the tab strip suppresses the "Delete
    /// session" affordance for them — same reason the backend refuses
    /// DELETE /api/sessions/:id for worker sessions. Always false for
    /// project tabs.
    pub is_worker: bool,
    /// True iff this tab refers to a session kicked off by a repeating
    /// task. The tab strip hides the "Clear session" affordance for
    /// these — clearing one would wipe the run's audit trail without
    /// removing the row, leaving a confusing empty stub. Backend
    /// enforces the policy via POST /clear → 409. Always false for
    /// project / report / repeating-task tabs.
    pub is_repeating_task_session: bool,
    /// True iff this tab refers to a temp session — one deleted outright
    /// when its last tab (across all users) is closed (see
    /// [`deletes_on_last_tab_close`]). The strip marks the chip and offers a
    /// "Keep session" action. Always false for non-session tabs.
    pub is_temp: bool,
    /// When the session tab's session was sealed (its card moved on; no
    /// agent runs in it again). `None` for live sessions and non-session tabs.
    pub sealed_at: Option<String>,
    /// Why it was sealed (`advanced`, `reviewed`, `moved`, `superseded`, …).
    pub sealed_reason: Option<String>,
}

#[derive(Deserialize)]
pub struct UpsertTabRequest {
    pub item_type: String,
    pub item_id: String,
}

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/me/tabs", get(list_tabs).post(upsert_tab))
        .route("/api/me/tabs/{item_type}/{item_id}", delete(delete_tab))
        .route("/api/me/use-cases", get(get_use_cases).put(put_use_cases))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

fn auth_user(req: &Request<Body>) -> &AuthUser {
    req.extensions()
        .get::<AuthUser>()
        .expect("auth middleware should inject AuthUser")
}

fn validate_item_type(s: &str) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    match s {
        "session" | "project" | "report" | "repeating_task" | "doc_review" | "terminal" => Ok(()),
        _ => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "item_type must be 'session', 'project', 'report', 'repeating_task', 'doc_review', or 'terminal'"
            })),
        )),
    }
}

/// A terminal tab's label: the terminal's name, but only for the user's own
/// open terminal — a terminal is a private shell, so another user's id
/// resolves to `None` (tab refused / filtered) like a deleted item.
async fn terminal_name(state: &AppState, user_id: &str, id: &str) -> Option<String> {
    state
        .db
        .get_terminal(id)
        .await
        .ok()
        .flatten()
        .filter(|t| t.closed_at.is_none() && t.user_id == user_id)
        .map(|t| t.name)
}

/// Reports are file-backed, identified by `<folder>/<file>` (the same
/// pair the `/api/reports/:folder/:file` route uses). The tab strip
/// stores that pair in `item_id` as `<folder>/<file>`; this helper
/// splits and validates the segments against the same charset the
/// reports route uses, then resolves the on-disk path so we can read
/// the report's title for the tab label.
fn split_report_id(item_id: &str) -> Option<(String, String)> {
    let (folder, file) = item_id.split_once('/')?;
    let folder = crate::routes::reports::safe_segment(folder, false)?;
    let file = crate::routes::reports::safe_segment(file, true)?;
    Some((folder, file))
}

/// Resolve a report tab's label. Reads the file's frontmatter for a
/// `title:` line; falls back to the file name (sans `.md`) when there
/// is no frontmatter. Returns `None` when the file is missing, which
/// causes the tab to be filtered out of GET /api/me/tabs.
fn report_label(state: &AppState, folder: &str, file: &str) -> Option<String> {
    let path = state
        .config
        .data_dir
        .join("reports")
        .join(folder)
        .join(file);
    let content = std::fs::read_to_string(&path).ok()?;
    Some(crate::routes::reports::report_title(&content, file))
}

/// GET /api/me/tabs — list this user's tabs, MRU first.
///
/// Each tab is enriched with the underlying session/project name so the
/// strip can render a label without cross-referencing a separate list.
/// Tabs whose referenced item no longer exists are filtered out — this
/// is the cross-device cleanup path for sessions/projects deleted on
/// another device.
///
/// Implementation note: the per-tab name lookup is N+1 against
/// `sessions` / `projects`. Acceptable for the expected tab count
/// (humans rarely keep more than a couple dozen open) and avoids a
/// hand-rolled polymorphic JOIN. If users start running into 100+
/// tabs we can swap this for a single batched lookup keyed by id.
async fn list_tabs(State(state): State<Arc<AppState>>, req: Request<Body>) -> impl IntoResponse {
    let user = auth_user(&req);
    let tabs = match state.db.list_user_tabs(&user.user_id).await {
        Ok(t) => t,
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            ));
        }
    };

    let mut out: Vec<TabView> = Vec::with_capacity(tabs.len());
    for t in tabs {
        let mut seal: (Option<String>, Option<String>) = (None, None);
        let (name, is_worker, is_repeating_task_session, is_temp) = match t.item_type.as_str() {
            "session" => match state.db.get_session(&t.item_id).await.ok().flatten() {
                Some(s) => {
                    let is_temp = deletes_on_last_tab_close(&s);
                    seal = (s.sealed_at, s.sealed_reason);
                    (
                        Some(s.name),
                        s.is_worker,
                        s.repeating_task_id.is_some(),
                        is_temp,
                    )
                }
                None => (None, false, false, false),
            },
            "project" => (
                state
                    .db
                    .get_project(&t.item_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|p| p.name),
                false,
                false,
                false,
            ),
            "repeating_task" => (
                state
                    .db
                    .get_repeating_task(&t.item_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|task| task.name),
                false,
                false,
                false,
            ),
            "doc_review" => (
                state
                    .db
                    .get_doc_review(&t.item_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|r| r.title),
                false,
                false,
                false,
            ),
            "terminal" => (
                terminal_name(&state, &user.user_id, &t.item_id).await,
                false,
                false,
                false,
            ),
            "report" => (
                split_report_id(&t.item_id)
                    .and_then(|(folder, file)| report_label(&state, &folder, &file)),
                false,
                false,
                false,
            ),
            _ => (None, false, false, false),
        };
        if let Some(name) = name {
            out.push(TabView {
                item_type: t.item_type,
                item_id: t.item_id,
                last_active: t.last_active,
                name,
                is_worker,
                is_repeating_task_session,
                is_temp,
                sealed_at: seal.0,
                sealed_reason: seal.1,
            });
        }
    }
    Ok(Json(out))
}

/// POST /api/me/tabs — open or activate a tab.
///
/// Idempotent: same body twice in a row just bumps `last_active`. This
/// is what the frontend calls when the user clicks any tab (including
/// one already in the strip), which both reorders MRU and counts as
/// "viewed" for the unread badge.
async fn upsert_tab(State(state): State<Arc<AppState>>, req: Request<Body>) -> impl IntoResponse {
    let user_id = auth_user(&req).user_id.clone();
    // Have to take the body separately because we already borrowed `req`.
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, 1024).await {
        Ok(b) => b,
        Err(_) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "body too large" })),
            ));
        }
    };
    let _ = parts; // unused
    let req_body: UpsertTabRequest = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid JSON: {e}") })),
            ));
        }
    };
    validate_item_type(&req_body.item_type)?;

    // Reports are file-backed, so the DB-layer existence check trusts
    // the route. Validate the on-disk path here before we write a tab
    // row — otherwise a stale URL would create an orphan tab no GET
    // can ever resolve.
    if req_body.item_type == "report" {
        let exists = split_report_id(&req_body.item_id)
            .and_then(|(folder, file)| report_label(&state, &folder, &file).map(|_| (folder, file)))
            .is_some();
        if !exists {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "referenced item does not exist" })),
            ));
        }
    }
    if req_body.item_type == "terminal"
        && terminal_name(&state, &user_id, &req_body.item_id)
            .await
            .is_none()
    {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "referenced item does not exist" })),
        ));
    }

    match state
        .db
        .upsert_user_tab(&user_id, &req_body.item_type, &req_body.item_id)
        .await
    {
        Ok(Some(tab)) => {
            // Look up the underlying name so the response shape matches
            // GET /api/me/tabs (same denormalized `name` field). The
            // upsert path has already verified the item exists, so a
            // missing name here would be a TOCTOU race — fall back to
            // empty rather than 500ing.
            let mut seal: (Option<String>, Option<String>) = (None, None);
            let (name, is_worker, is_repeating_task_session, is_temp) = match tab.item_type.as_str()
            {
                "session" => match state.db.get_session(&tab.item_id).await.ok().flatten() {
                    Some(s) => {
                        let is_temp = deletes_on_last_tab_close(&s);
                        seal = (s.sealed_at, s.sealed_reason);
                        (s.name, s.is_worker, s.repeating_task_id.is_some(), is_temp)
                    }
                    None => (String::new(), false, false, false),
                },
                "project" => (
                    state
                        .db
                        .get_project(&tab.item_id)
                        .await
                        .ok()
                        .flatten()
                        .map(|p| p.name)
                        .unwrap_or_default(),
                    false,
                    false,
                    false,
                ),
                "repeating_task" => (
                    state
                        .db
                        .get_repeating_task(&tab.item_id)
                        .await
                        .ok()
                        .flatten()
                        .map(|task| task.name)
                        .unwrap_or_default(),
                    false,
                    false,
                    false,
                ),
                "doc_review" => (
                    state
                        .db
                        .get_doc_review(&tab.item_id)
                        .await
                        .ok()
                        .flatten()
                        .map(|r| r.title)
                        .unwrap_or_default(),
                    false,
                    false,
                    false,
                ),
                "terminal" => (
                    terminal_name(&state, &user_id, &tab.item_id)
                        .await
                        .unwrap_or_default(),
                    false,
                    false,
                    false,
                ),
                "report" => (
                    split_report_id(&tab.item_id)
                        .and_then(|(folder, file)| report_label(&state, &folder, &file))
                        .unwrap_or_default(),
                    false,
                    false,
                    false,
                ),
                _ => (String::new(), false, false, false),
            };
            Ok(Json(TabView {
                item_type: tab.item_type,
                item_id: tab.item_id,
                last_active: tab.last_active,
                name,
                is_worker,
                is_repeating_task_session,
                is_temp,
                sealed_at: seal.0,
                sealed_reason: seal.1,
            }))
        }
        // Item doesn't exist — refuse to create the tab. Stops phantom
        // chips from being written for stale URLs or cross-device delete
        // races. The frontend treats 404 here as "drop the local tab".
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "referenced item does not exist" })),
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}

/// DELETE /api/me/tabs/:item_type/:item_id — close a tab. Does not touch
/// the underlying item — EXCEPT for temp sessions, which are deleted
/// outright when their last tab (across all users) goes away.
async fn delete_tab(
    State(state): State<Arc<AppState>>,
    Path((item_type, item_id)): Path<(String, String)>,
    req: Request<Body>,
) -> impl IntoResponse {
    let user_id = auth_user(&req).user_id.clone();
    validate_item_type(&item_type)?;

    if let Err(e) = state
        .db
        .delete_user_tab(&user_id, &item_type, &item_id)
        .await
    {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ));
    }

    // Temp sessions live only as long as some tab points at them: when the
    // last tab closes, delete the session — same cleanup +
    // `session-deleted` broadcast as DELETE /api/sessions/:id. Failures are
    // logged, never surfaced: closing a tab must succeed even when the temp
    // cleanup can't run (the startup sweep in `routes::sessions` converges
    // leftovers on the next boot).
    if item_type == "session" {
        maybe_delete_temp_session(&state, &item_id).await;
    }

    Ok(StatusCode::NO_CONTENT)
}

/// Whether closing the last tab on `session` deletes it: a temp session
/// the user owns the lifecycle of. Workers belong to the orchestrator, and
/// expert sessions to the core service or plugin that spawned them (e.g. the
/// Project Planner's interview session, which it keeps driving after the
/// user peeks at it) — viewing one in a tab and closing it must not kill it.
/// The startup orphan sweep still reclaims tab-less temp sessions.
fn deletes_on_last_tab_close(session: &crate::db::models::Session) -> bool {
    session.is_temp && !session.is_worker && !session.is_expert
}

/// Delete `session_id` iff it is a temp session with zero remaining tabs
/// whose lifecycle is the user's ([`deletes_on_last_tab_close`]).
/// Part of the tab-close path — see [`delete_tab`].
async fn maybe_delete_temp_session(state: &AppState, session_id: &str) {
    let session = match state.db.get_session(session_id).await {
        Ok(Some(s)) => s,
        // Already gone (double-close race) — nothing to do.
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(session_id = %session_id, "Temp-session check failed: {e}");
            return;
        }
    };
    if !deletes_on_last_tab_close(&session) {
        return;
    }
    match state
        .db
        .count_user_tabs_for_item("session", session_id)
        .await
    {
        Ok(0) => match crate::routes::sessions::delete_session_core(state, session_id).await {
            Ok(_) => {
                tracing::info!(session_id = %session_id, "Deleted temp session on last tab close");
            }
            Err(e) => {
                tracing::warn!(session_id = %session_id, "Failed to delete temp session on last tab close: {e}");
            }
        },
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(session_id = %session_id, "Temp-session tab count failed: {e}");
        }
    }
}

// ── Use cases ─────────────────────────────────────────────────────────

/// Plugin-store collection holding each user's answer to the setup wizard's
/// "What will you use PeckBoard for?", keyed by user id (same KV the app
/// settings use — no table of its own).
const USE_CASES_COLLECTION: &str = "user_use_cases";

/// The use cases a user can pick. The web app's `store/useCases.ts` keeps the
/// labels; anything else in a PUT is a 400.
const USE_CASES: &[&str] = &["coding", "writing", "ops", "printing", "other"];

#[derive(Deserialize)]
struct UseCasesBody {
    use_cases: Vec<String>,
}

/// GET /api/me/use-cases → `{"use_cases": [...]}` (empty when never answered).
async fn get_use_cases(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
) -> impl IntoResponse {
    let db = state.db.clone();
    let raw = tokio::task::spawn_blocking(move || {
        db.plugin_store_get_blocking(
            crate::routes::settings::SETTINGS_NS,
            USE_CASES_COLLECTION,
            &user.user_id,
        )
    })
    .await;
    let use_cases: Vec<String> = match raw {
        Ok(Ok(Some(json))) => serde_json::from_str(&json).unwrap_or_default(),
        _ => Vec::new(),
    };
    Json(serde_json::json!({ "use_cases": use_cases }))
}

/// PUT /api/me/use-cases `{"use_cases": [...]}` → 204. Replaces the caller's
/// own answer (deduplicated, in the fixed [`USE_CASES`] order).
async fn put_use_cases(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Json(body): Json<UseCasesBody>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    if let Some(bad) = body
        .use_cases
        .iter()
        .find(|u| !USE_CASES.contains(&u.as_str()))
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("unknown use case '{bad}' (expected one of {})", USE_CASES.join(", "))
            })),
        ));
    }
    let picked: Vec<&str> = USE_CASES
        .iter()
        .copied()
        .filter(|u| body.use_cases.iter().any(|b| b == u))
        .collect();
    let value = serde_json::to_string(&picked).unwrap_or_else(|_| "[]".into());
    let db = state.db.clone();
    let res = tokio::task::spawn_blocking(move || {
        db.plugin_store_put_blocking(
            crate::routes::settings::SETTINGS_NS,
            USE_CASES_COLLECTION,
            &user.user_id,
            &value,
        )
    })
    .await;
    match res {
        Ok(Ok(_)) => Ok(StatusCode::NO_CONTENT),
        Ok(Err(e)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::middleware::tests::{
        seed_authenticated_user, seed_authenticated_user_with_suffix, test_state,
    };
    use axum::http::header;
    use tower::ServiceExt;

    async fn call(
        state: &Arc<AppState>,
        token: &str,
        method: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let builder = Request::builder()
            .method(method)
            .uri("/api/me/use-cases")
            .header(header::AUTHORIZATION, format!("Bearer {token}"));
        let req = match body {
            Some(v) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let app = Router::new()
            .merge(router(state.clone()))
            .with_state(state.clone());
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    /// Each user's answer is their own, round-trips in a fixed order, and
    /// an unknown value is refused.
    #[tokio::test]
    async fn use_cases_are_per_user_and_validated() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let alice = seed_authenticated_user(&state, "admin").await;
        let bob = seed_authenticated_user_with_suffix(&state, "user", "bob").await;

        let (status, body) = call(&state, &alice, "GET", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["use_cases"], serde_json::json!([]));

        let (status, _) = call(
            &state,
            &alice,
            "PUT",
            Some(serde_json::json!({ "use_cases": ["printing", "coding", "coding"] })),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, body) = call(&state, &alice, "GET", None).await;
        assert_eq!(body["use_cases"], serde_json::json!(["coding", "printing"]));

        let (_, body) = call(&state, &bob, "GET", None).await;
        assert_eq!(body["use_cases"], serde_json::json!([]), "not shared");

        let (status, _) = call(
            &state,
            &bob,
            "PUT",
            Some(serde_json::json!({ "use_cases": ["gardening"] })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
