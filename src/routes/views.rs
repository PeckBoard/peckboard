//! `/api/me/views` — the user's saved widget dashboards.
//!
//! A view is `{id, name, created_at, updated_at, widgets, terminals,
//! projects, cards}` where `widgets` are [`ViewWidget`] rects on a
//! 12-column grid (session / terminal panes, project summaries, notes,
//! reports, and the info widgets in `WidgetKind`); `terminals` maps each
//! referenced terminal id to its display identity (including soft-closed
//! ones, so a widget can offer "Reopen on <host>"), `projects` each
//! referenced project id (pane or scope) to `{name}`, and `cards` each
//! referenced card id to `{title}`. Views are strictly per-user: another
//! user's view id answers 404, exactly like a missing one. Widgets may only
//! reference sessions the caller may open and the caller's own terminals
//! (admins: any), and existing projects / cards (a card must belong to the
//! widget's project) — see [`views::check_widget_refs`]. A legacy `layout`
//! split tree is still accepted in request bodies and converted to widgets.
//! Every write sends the owner's open tabs a `views-changed` frame.

use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    middleware,
    routing::get,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::auth::middleware::{AuthUser, require_auth};
use crate::db::Db;
use crate::db::crud::{ViewLayout, ViewWidget, validate_view_name, validate_widgets};
use crate::db::models::SessionView;
use crate::service::views::{self, ViewAction};
use crate::state::AppState;

type ApiError = (StatusCode, Json<serde_json::Value>);

#[derive(Deserialize)]
struct CreateViewRequest {
    name: String,
    #[serde(default)]
    widgets: Option<Vec<ViewWidget>>,
    /// Legacy split tree; converted to widgets.
    #[serde(default)]
    layout: Option<ViewLayout>,
}

#[derive(Deserialize)]
struct UpdateViewRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    widgets: Option<Vec<ViewWidget>>,
    /// Legacy split tree; converted to widgets.
    #[serde(default)]
    layout: Option<ViewLayout>,
}

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/me/views", get(list_views).post(create_view))
        .route(
            "/api/me/views/{id}",
            get(get_view).put(update_view).delete(delete_view),
        )
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

fn err(status: StatusCode, msg: impl Into<String>) -> ApiError {
    (status, Json(serde_json::json!({ "error": msg.into() })))
}

fn internal(e: anyhow::Error) -> ApiError {
    err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn not_found() -> ApiError {
    err(StatusCode::NOT_FOUND, "view not found")
}

/// Parse a JSON body, turning malformed input into a 400 with the serde
/// reason (the stock `Json` extractor answers 422 in plain text).
fn parse_body<T: serde::de::DeserializeOwned>(bytes: &Bytes) -> Result<T, ApiError> {
    serde_json::from_slice(bytes)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("invalid JSON: {e}")))
}

/// The validated widget set a request asks for: `widgets` as sent, or a
/// legacy `layout` tree converted to widgets. `None` when neither is set.
fn requested_widgets(
    widgets: Option<Vec<ViewWidget>>,
    layout: Option<ViewLayout>,
) -> Result<Option<Vec<ViewWidget>>, ApiError> {
    let bad = |e: String| err(StatusCode::BAD_REQUEST, e);
    let widgets = match (widgets, layout) {
        (Some(_), Some(_)) => return Err(bad("send widgets or layout, not both".into())),
        (Some(w), None) => w,
        (None, Some(layout)) => {
            layout.validate().map_err(bad)?;
            layout.to_widgets()
        }
        (None, None) => return Ok(None),
    };
    validate_widgets(&widgets).map_err(bad)?;
    Ok(Some(widgets))
}
/// [`views::check_widget_refs`] as an HTTP answer: a missing or foreign
/// ref is a 404 naming the kind, like `/api/terminals/{id}`.
async fn check_widget_refs(
    db: &Db,
    user: &AuthUser,
    widgets: &[ViewWidget],
) -> Result<(), ApiError> {
    views::check_widget_refs(db, &user.user_id, user.role == "admin", widgets)
        .await
        .map_err(|e| match e {
            views::RefError::NotFound(m) => err(StatusCode::NOT_FOUND, m),
            views::RefError::Internal(e) => internal(e),
        })
}

async fn full_json(
    state: &AppState,
    user: &AuthUser,
    view: (SessionView, Vec<ViewWidget>),
) -> Result<Json<serde_json::Value>, ApiError> {
    views::full_json(&state.db, &user.user_id, user.role == "admin", view)
        .await
        .map(Json)
        .map_err(internal)
}

/// The writing browser tab's id (`X-Peckboard-Tab`), echoed on
/// `views-changed` so that tab skips its own autosave.
fn origin_tab(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("x-peckboard-tab")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= 64)
}

/// GET /api/me/views — `[{id, name, created_at, updated_at}]`, oldest first.
async fn list_views(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let list = state
        .db
        .list_session_views(&user.user_id)
        .await
        .map_err(internal)?;
    Ok(Json(serde_json::Value::Array(
        list.iter().map(views::summary_json).collect(),
    )))
}

/// POST /api/me/views `{name, widgets}` (or legacy `{name, layout}`) — the
/// created view with its widgets.
async fn create_view(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let req: CreateViewRequest = parse_body(&body)?;
    let name = validate_view_name(&req.name).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    let widgets = requested_widgets(req.widgets, req.layout)?.unwrap_or_default();
    check_widget_refs(&state.db, &user, &widgets).await?;
    let view = state
        .db
        .create_session_view(&user.user_id, &name, widgets)
        .await
        .map_err(internal)?;
    views::notify_changed(
        &state.broadcaster,
        &user.user_id,
        &view.0.id,
        ViewAction::Created,
        origin_tab(&headers),
    );
    Ok((StatusCode::CREATED, full_json(&state, &user, view).await?))
}

/// GET /api/me/views/:id — the full view (see [`views::full_json`]).
async fn get_view(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let view = state
        .db
        .get_session_view(&user.user_id, &id)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)?;
    full_json(&state, &user, view).await
}

/// PUT /api/me/views/:id `{name?, widgets?}` (or legacy `layout?`) —
/// rename and/or replace every widget; answers the full updated view.
async fn update_view(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    let req: UpdateViewRequest = parse_body(&body)?;
    let name = req
        .name
        .as_deref()
        .map(validate_view_name)
        .transpose()
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    let widgets = requested_widgets(req.widgets, req.layout)?;
    if let Some(widgets) = &widgets {
        check_widget_refs(&state.db, &user, widgets).await?;
    }
    let view = state
        .db
        .update_session_view(&user.user_id, &id, name, widgets)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)?;
    views::notify_changed(
        &state.broadcaster,
        &user.user_id,
        &id,
        ViewAction::Updated,
        origin_tab(&headers),
    );
    full_json(&state, &user, view).await
}

/// DELETE /api/me/views/:id — 204, or 404 when it isn't the user's.
async fn delete_view(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    if state
        .db
        .delete_session_view(&user.user_id, &id)
        .await
        .map_err(internal)?
    {
        views::notify_changed(
            &state.broadcaster,
            &user.user_id,
            &id,
            ViewAction::Deleted,
            origin_tab(&headers),
        );
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(not_found())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::crud::WidgetKind;
    use crate::db::models::{NewUser, Terminal};
    fn user(id: &str, role: &str) -> AuthUser {
        AuthUser {
            user_id: id.into(),
            role: role.into(),
            session_id: "s".into(),
        }
    }

    fn term_widget(id: &str) -> Vec<ViewWidget> {
        vec![ViewWidget {
            id: "w1".into(),
            kind: WidgetKind::Terminal,
            x: 0,
            y: 0,
            w: 6,
            h: 8,
            terminal_id: Some(id.into()),
            ..Default::default()
        }]
    }

    #[tokio::test]
    async fn widgets_may_only_reference_the_callers_terminals() {
        let db = Db::in_memory().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        for u in ["owner", "other"] {
            db.create_user(NewUser {
                id: u.into(),
                username: u.into(),
                email: None,
                password_hash: "h".into(),
                role: "user".into(),
                created_at: now.clone(),
                updated_at: now.clone(),
            })
            .await
            .unwrap();
        }
        db.insert_terminal(Terminal {
            id: "t1".into(),
            user_id: "owner".into(),
            plugin_id: "ssh".into(),
            host_id: "h".into(),
            name: "t1".into(),
            host_label: "me@box:22".into(),
            tmux_session: "peck-t1".into(),
            persistent: false,
            created_at: now.clone(),
            last_active_at: now,
            closed_at: None,
        })
        .await
        .unwrap();

        let widgets = term_widget("t1");
        assert!(
            check_widget_refs(&db, &user("owner", "user"), &widgets)
                .await
                .is_ok()
        );
        assert!(
            check_widget_refs(&db, &user("other", "admin"), &widgets)
                .await
                .is_ok()
        );
        let (status, _) = check_widget_refs(&db, &user("other", "user"), &widgets)
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = check_widget_refs(&db, &user("owner", "user"), &term_widget("nope"))
            .await
            .unwrap_err();
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "unknown answers like foreign"
        );

        // A soft-closed terminal stays referenceable (reopen placeholder).
        db.close_terminal("t1").await.unwrap();
        assert!(
            check_widget_refs(&db, &user("owner", "user"), &widgets)
                .await
                .is_ok()
        );

        // Unknown projects answer 404.
        let mut proj = term_widget("t1");
        proj[0].kind = WidgetKind::Project;
        proj[0].terminal_id = None;
        proj[0].project_id = Some("missing".into());
        let (status, _) = check_widget_refs(&db, &user("owner", "user"), &proj)
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn card_refs_must_exist_in_the_widgets_project() {
        use crate::db::models::{NewCard, NewFolder, NewProject};
        let db = Db::in_memory().unwrap();
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        for p in ["p1", "p2"] {
            db.create_project(NewProject {
                id: p.into(),
                name: p.into(),
                context: "".into(),
                folder_id: "f".into(),
                worker_count: 1,
                status: "active".into(),
                workflow: "task".into(),
                model: None,
                effort: None,
                parallel_instructions: false,
                auto_notify_changes: true,
                worker_communication: false,
                created_at: ts.clone(),
                last_accessed_at: ts.clone(),
                budget_usd_cents: None,
                budget_period: None,
                worktree_isolation: false,
            })
            .await
            .unwrap();
        }
        db.create_card(NewCard {
            id: "c1".into(),
            project_id: "p1".into(),
            title: "C".into(),
            description: "".into(),
            step: "todo".into(),
            priority: 1,
            workflow: "task".into(),
            model: None,
            effort: None,
            blocked: false,
            block_reason: None,
            created_at: ts.clone(),
            updated_at: ts,
            system_prompt_name: None,
        })
        .await
        .unwrap();
        let dep = |project: &str, card: &str| {
            vec![ViewWidget {
                id: "w".into(),
                kind: WidgetKind::Dependencies,
                x: 0,
                y: 0,
                w: 6,
                h: 9,
                project_id: Some(project.into()),
                card_id: Some(card.into()),
                ..Default::default()
            }]
        };
        let u = user("u", "user");
        assert!(check_widget_refs(&db, &u, &dep("p1", "c1")).await.is_ok());
        for (p, c) in [("p2", "c1"), ("p1", "nope")] {
            let (status, Json(body)) = check_widget_refs(&db, &u, &dep(p, c)).await.unwrap_err();
            assert_eq!(status, StatusCode::NOT_FOUND);
            assert_eq!(body["error"], "card not found");
        }
    }

    #[tokio::test]
    async fn session_refs_must_be_sessions_the_caller_may_open() {
        use crate::db::models::{NewFolder, NewSession};
        let db = Db::in_memory().unwrap();
        let ts = chrono::Utc::now().to_rfc3339();
        db.create_folder(NewFolder {
            id: "f".into(),
            name: "F".into(),
            path: "/tmp/f".into(),
            created_at: ts.clone(),
        })
        .await
        .unwrap();
        db.create_session(NewSession {
            id: "theirs".into(),
            name: "theirs".into(),
            folder_id: "f".into(),
            created_at: ts.clone(),
            last_activity: ts,
            user_id: Some("other".into()),
            ..Default::default()
        })
        .await
        .unwrap();
        let w = vec![ViewWidget {
            id: "w".into(),
            kind: WidgetKind::Session,
            x: 0,
            y: 0,
            w: 6,
            h: 10,
            session_id: Some("theirs".into()),
            ..Default::default()
        }];
        let (status, Json(body)) = check_widget_refs(&db, &user("me", "user"), &w)
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "session not found");
        assert!(
            check_widget_refs(&db, &user("other", "user"), &w)
                .await
                .is_ok()
        );
        assert!(
            check_widget_refs(&db, &user("me", "admin"), &w)
                .await
                .is_ok()
        );
    }
}
