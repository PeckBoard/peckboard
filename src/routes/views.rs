//! `/api/me/views` — the user's saved widget dashboards.
//!
//! A view is `{id, name, created_at, updated_at, widgets, terminals,
//! projects}` where `widgets` are [`ViewWidget`] rects on a 12-column grid
//! showing a session, an SSH terminal, or a project summary; `terminals`
//! maps each referenced terminal id to its display identity (including
//! soft-closed ones, so a widget can offer "Reopen on <host>") and
//! `projects` each referenced project id to `{name}`. Views are strictly
//! per-user: another user's view id answers 404, exactly like a missing
//! one. Widgets may only reference the caller's own terminals (admins:
//! any), the same rule as attaching, and existing projects. A legacy
//! `layout` split tree is still accepted in request bodies and converted
//! to widgets.

use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    middleware,
    routing::get,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::auth::middleware::{AuthUser, require_auth};
use crate::db::Db;
use crate::db::crud::{
    ViewLayout, ViewWidget, WidgetKind, validate_view_name, validate_widgets, widget_refs,
};
use crate::db::models::{SessionView, Terminal};
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

fn may_use_terminal(user: &AuthUser, t: &Terminal) -> bool {
    t.user_id == user.user_id || user.role == "admin"
}

/// Every terminal the widgets reference must exist and be the caller's
/// (admins: anyone's), and every project must exist. Unknown and foreign
/// terminal ids answer the same 404, like `/api/terminals/{id}`.
/// Soft-closed terminals are allowed — their widget shows a reopen
/// placeholder.
async fn check_widget_refs(
    db: &Db,
    user: &AuthUser,
    widgets: &[ViewWidget],
) -> Result<(), ApiError> {
    for id in widget_refs(widgets, WidgetKind::Terminal) {
        match db.get_terminal(id).await.map_err(internal)? {
            Some(t) if may_use_terminal(user, &t) => {}
            _ => return Err(err(StatusCode::NOT_FOUND, "terminal not found")),
        }
    }
    let projects = widget_refs(widgets, WidgetKind::Project);
    if !projects.is_empty() {
        let found = db
            .project_names(projects.iter().map(|s| s.to_string()).collect())
            .await
            .map_err(internal)?;
        if projects.iter().any(|p| !found.contains_key(*p)) {
            return Err(err(StatusCode::NOT_FOUND, "project not found"));
        }
    }
    Ok(())
}

fn summary_json(v: &SessionView) -> serde_json::Value {
    serde_json::json!({
        "id": v.id,
        "name": v.name,
        "created_at": v.created_at,
        "updated_at": v.updated_at,
    })
}

/// The full view, plus `terminals: {id: {name, host_label, plugin_id,
/// host_id, closed}}` and `projects: {id: {name}}` for the targets its
/// widgets reference.
async fn full_json(
    state: &AppState,
    user: &AuthUser,
    (v, widgets): (SessionView, Vec<ViewWidget>),
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut terminals = serde_json::Map::new();
    for id in widget_refs(&widgets, WidgetKind::Terminal) {
        if let Some(t) = state.db.get_terminal(id).await.map_err(internal)?
            && may_use_terminal(user, &t)
        {
            terminals.insert(
                t.id.clone(),
                serde_json::json!({
                    "name": t.name,
                    "host_label": t.host_label,
                    "plugin_id": t.plugin_id,
                    "host_id": t.host_id,
                    "closed": t.closed_at.is_some(),
                }),
            );
        }
    }
    let project_ids: Vec<String> = widget_refs(&widgets, WidgetKind::Project)
        .into_iter()
        .map(str::to_string)
        .collect();
    let projects: serde_json::Map<String, serde_json::Value> = if project_ids.is_empty() {
        serde_json::Map::new()
    } else {
        state
            .db
            .project_names(project_ids)
            .await
            .map_err(internal)?
            .into_iter()
            .map(|(id, name)| (id, serde_json::json!({ "name": name })))
            .collect()
    };
    let mut out = summary_json(&v);
    out["widgets"] = serde_json::json!(widgets);
    out["terminals"] = serde_json::Value::Object(terminals);
    out["projects"] = serde_json::Value::Object(projects);
    Ok(Json(out))
}

/// GET /api/me/views — `[{id, name, created_at, updated_at}]`, oldest first.
async fn list_views(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let views = state
        .db
        .list_session_views(&user.user_id)
        .await
        .map_err(internal)?;
    Ok(Json(serde_json::Value::Array(
        views.iter().map(summary_json).collect(),
    )))
}

/// POST /api/me/views `{name, widgets}` (or legacy `{name, layout}`) — the
/// created view with its widgets.
async fn create_view(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
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
    Ok((StatusCode::CREATED, full_json(&state, &user, view).await?))
}

/// GET /api/me/views/:id — the full view (see [`full_json`]).
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
    full_json(&state, &user, view).await
}

/// DELETE /api/me/views/:id — 204, or 404 when it isn't the user's.
async fn delete_view(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    if state
        .db
        .delete_session_view(&user.user_id, &id)
        .await
        .map_err(internal)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(not_found())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::NewUser;

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
            session_id: None,
            terminal_id: Some(id.into()),
            project_id: None,
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
}
