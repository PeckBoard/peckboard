//! `/api/plugin-ui/*` — the **authenticated**, plugin-owned app-UI surface.
//!
//! Unlike `/plugin-api/*` (public, plugin-self-authenticated — see
//! [`crate::routes::plugin_api`]), this prefix sits BEHIND `require_auth`: every
//! request is a logged-in user's. Core authenticates, then hands the request to
//! the owning plugin via
//! [`PluginManager::serve_http_authed`](crate::plugin::manager::PluginManager::serve_http_authed),
//! which sets a trusted user-authority context so the plugin's scoped host
//! functions may act on the user's behalf. A plugin claims a route by declaring
//! it in its manifest `ui_routes` (+ the `http.request.authed` hook and the
//! `user_authority` permission). Core stays generic — it knows nothing about
//! any specific endpoint.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{OriginalUri, State},
    http::{HeaderName, HeaderValue, Method, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::any,
};

use crate::auth::middleware::{AuthUser, require_auth};
use crate::plugin::hooks::PluginHttpOutcome;
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/plugin-ui", any(serve))
        .route("/api/plugin-ui/{*rest}", any(serve))
        // Same auth as every other `/api/*` route: a logged-in user is required.
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// Dispatch an authenticated request to the owning plugin (under the user's
/// authority) and translate its response — or the absence of one — to Axum.
async fn serve(
    State(state): State<Arc<AppState>>,
    axum::Extension(user): axum::Extension<AuthUser>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path().to_string();
    let query = uri.query().unwrap_or("").to_string();

    let mut header_map: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in headers.iter() {
        if let Ok(v) = value.to_str() {
            header_map
                .entry(name.as_str().to_string())
                .and_modify(|existing| {
                    existing.push_str(", ");
                    existing.push_str(v);
                })
                .or_insert_with(|| v.to_string());
        }
    }

    let body_str = String::from_utf8_lossy(&body).into_owned();

    match state
        .plugins
        .serve_http_authed(
            &user.user_id,
            method.as_str(),
            &path,
            &query,
            &header_map,
            &body_str,
        )
        .await
    {
        PluginHttpOutcome::Served {
            status,
            headers,
            body,
        } => {
            let mut response = Response::new(axum::body::Body::from(body));
            *response.status_mut() =
                StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let out_headers = response.headers_mut();
            for (name, value) in headers {
                if let (Ok(name), Ok(value)) = (
                    HeaderName::from_bytes(name.as_bytes()),
                    HeaderValue::from_str(&value),
                ) {
                    out_headers.append(name, value);
                }
            }
            response
        }
        PluginHttpOutcome::NoRoute => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no plugin route matches" })),
        )
            .into_response(),
    }
}
