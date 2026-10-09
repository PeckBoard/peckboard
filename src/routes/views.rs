//! `/api/me/views` — the user's saved multi-session split views.
//!
//! A view is `{id, name, created_at, updated_at, layout, terminals}` where
//! `layout` is a [`ViewLayout`] tree whose leaves show a session or an SSH
//! terminal, and `terminals` maps each referenced terminal id to its display
//! identity (including soft-closed ones, so a pane can offer "Reopen on
//! <host>"). Views are strictly per-user: another user's view id answers
//! 404, exactly like a missing one. A layout may only reference the
//! caller's own terminals (admins: any), the same rule as attaching.

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
use crate::db::crud::{ViewLayout, validate_view_name};
use crate::db::models::{SessionView, Terminal};
use crate::state::AppState;

type ApiError = (StatusCode, Json<serde_json::Value>);

#[derive(Deserialize)]
struct CreateViewRequest {
    name: String,
    layout: ViewLayout,
}

#[derive(Deserialize)]
struct UpdateViewRequest {
    #[serde(default)]
    name: Option<String>,
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

fn validate_layout(layout: &ViewLayout) -> Result<(), ApiError> {
    layout
        .validate()
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))
}

fn may_use_terminal(user: &AuthUser, t: &Terminal) -> bool {
    t.user_id == user.user_id || user.role == "admin"
}

/// Every terminal the layout references must exist and be the caller's
/// (admins: anyone's). Unknown and foreign ids answer the same 404, like
/// `/api/terminals/{id}`. Soft-closed terminals are allowed — their pane
/// shows a reopen placeholder.
async fn check_terminal_access(
    db: &Db,
    user: &AuthUser,
    layout: &ViewLayout,
) -> Result<(), ApiError> {
    for id in layout.terminal_ids() {
        match db.get_terminal(id).await.map_err(internal)? {
            Some(t) if may_use_terminal(user, &t) => {}
            _ => return Err(err(StatusCode::NOT_FOUND, "terminal not found")),
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
/// host_id, closed}}` for the terminals its panes reference.
async fn full_json(
    state: &AppState,
    user: &AuthUser,
    (v, layout): (SessionView, ViewLayout),
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut terminals = serde_json::Map::new();
    for id in layout.terminal_ids() {
        if terminals.contains_key(id) {
            continue;
        }
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
    let mut out = summary_json(&v);
    out["layout"] = serde_json::json!(layout);
    out["terminals"] = serde_json::Value::Object(terminals);
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

/// POST /api/me/views `{name, layout}` — the created view with its layout.
async fn create_view(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    body: Bytes,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let req: CreateViewRequest = parse_body(&body)?;
    let name = validate_view_name(&req.name).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    validate_layout(&req.layout)?;
    check_terminal_access(&state.db, &user, &req.layout).await?;
    let view = state
        .db
        .create_session_view(&user.user_id, &name, req.layout)
        .await
        .map_err(internal)?;
    Ok((StatusCode::CREATED, full_json(&state, &user, view).await?))
}

/// GET /api/me/views/:id — `{id, name, created_at, updated_at, layout}`.
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

/// PUT /api/me/views/:id `{name?, layout?}` — rename and/or replace the
/// whole layout; answers the full updated view.
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
    if let Some(layout) = &req.layout {
        validate_layout(layout)?;
        check_terminal_access(&state.db, &user, layout).await?;
    }
    let view = state
        .db
        .update_session_view(&user.user_id, &id, name, req.layout)
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

    fn term_leaf(id: &str) -> ViewLayout {
        ViewLayout::Leaf {
            session_id: None,
            terminal_id: Some(id.into()),
        }
    }

    #[tokio::test]
    async fn layouts_may_only_reference_the_callers_terminals() {
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

        let layout = term_leaf("t1");
        assert!(
            check_terminal_access(&db, &user("owner", "user"), &layout)
                .await
                .is_ok()
        );
        assert!(
            check_terminal_access(&db, &user("other", "admin"), &layout)
                .await
                .is_ok()
        );
        let (status, _) = check_terminal_access(&db, &user("other", "user"), &layout)
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = check_terminal_access(&db, &user("owner", "user"), &term_leaf("nope"))
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
            check_terminal_access(&db, &user("owner", "user"), &layout)
                .await
                .is_ok()
        );
    }
}
