//! `/api/me/views` — the user's saved multi-session split views.
//!
//! A view is `{id, name, created_at, updated_at, layout}` where `layout`
//! is a [`ViewLayout`] tree. Views are strictly per-user: another user's
//! view id answers 404, exactly like a missing one.

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
use crate::db::crud::{ViewLayout, validate_view_name};
use crate::db::models::SessionView;
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

fn summary_json(v: &SessionView) -> serde_json::Value {
    serde_json::json!({
        "id": v.id,
        "name": v.name,
        "created_at": v.created_at,
        "updated_at": v.updated_at,
    })
}

fn full_json((v, layout): (SessionView, ViewLayout)) -> Json<serde_json::Value> {
    let mut out = summary_json(&v);
    out["layout"] = serde_json::json!(layout);
    Json(out)
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
    let view = state
        .db
        .create_session_view(&user.user_id, &name, req.layout)
        .await
        .map_err(internal)?;
    Ok((StatusCode::CREATED, full_json(view)))
}

/// GET /api/me/views/:id — `{id, name, created_at, updated_at, layout}`.
async fn get_view(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .db
        .get_session_view(&user.user_id, &id)
        .await
        .map_err(internal)?
        .map(full_json)
        .ok_or_else(not_found)
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
    }
    state
        .db
        .update_session_view(&user.user_id, &id, name, req.layout)
        .await
        .map_err(internal)?
        .map(full_json)
        .ok_or_else(not_found)
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
