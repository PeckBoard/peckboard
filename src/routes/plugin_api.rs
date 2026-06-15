//! `/plugin-api/*` — the public, plugin-owned HTTP surface.
//!
//! This prefix is mounted WITHOUT the `require_auth` middleware that
//! guards every `/api/*` route: the serving plugin owns authentication
//! for its own routes (e.g. API keys). Core does no auth here and has
//! zero knowledge of any specific endpoint — every request is handed to
//! [`PluginManager::serve_http`](crate::plugin::manager::PluginManager::serve_http),
//! which dispatches to whichever loaded plugin declared a matching
//! `http_routes` entry. If no plugin claims the path, the request 404s.
//!
//! This is kept strictly separate from `/api/*`: it adds a brand-new
//! prefix and touches none of the existing auth layers, so existing
//! `/api/*` protection is unchanged.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{OriginalUri, State},
    http::{HeaderName, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
};

use crate::plugin::hooks::PluginHttpOutcome;
use crate::state::AppState;

pub fn router(_state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        // The bare prefix and everything beneath it, for any method.
        .route("/plugin-api", any(serve))
        .route("/plugin-api/{*rest}", any(serve))
}

/// Dispatch a public request to the owning plugin and translate its
/// response (or the absence of one) into an Axum response.
async fn serve(
    State(state): State<Arc<AppState>>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path().to_string();
    let query = uri.query().unwrap_or("").to_string();

    // Header names are already lowercase (the `http` crate normalizes
    // them). Collapse duplicate values with `", "`, matching how a
    // server would re-serialize a multi-valued header.
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
        .serve_http(method.as_str(), &path, &query, &header_map, &body_str)
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
