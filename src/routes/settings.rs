//! `/api/settings/*` — app-level settings surfaced to the authenticated user.
//!
//! - Approved commands: the programs granted a persistent "always" approval
//!   by `run_command` (stored under the `core.common-tools` plugin id,
//!   `cli_always` collection). The UI reads them so grants can be reviewed
//!   and revoked.
//! - Caveman mode: global output-style level (`off` | `lite` | `full`) for
//!   interactive sessions, stored in the same plugin-store KV under
//!   [`SETTINGS_NS`]/[`SETTINGS_COLLECTION`] and read at dispatch time by
//!   `SessionManager::send_message_locked`.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{delete, get},
};
use std::sync::Arc;

use crate::auth::middleware::require_auth;
use crate::state::AppState;

/// Plugin id / collection the native `run_command` tool records "always"
/// approvals under (see `service::mcp_server::common_tools`).
const NS: &str = "core.common-tools";
const ALWAYS_COLLECTION: &str = "cli_always";

/// Plugin-store namespace for core app settings (shared with the dispatch
/// path in `provider::manager`, which reads `caveman_mode` per turn).
pub const SETTINGS_NS: &str = "core.settings";
pub const SETTINGS_COLLECTION: &str = "app";

const CAVEMAN_LEVELS: &[&str] = &["off", "lite", "full"];

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/settings/approved-commands", get(list_approved))
        .route(
            "/api/settings/approved-commands/{program}",
            delete(delete_approved),
        )
        .route("/api/settings/caveman", get(get_caveman).put(set_caveman))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// GET /api/settings/caveman → `{"level":"off|lite|full"}` (default "off").
async fn get_caveman(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let db = state.db.clone();
    let raw = tokio::task::spawn_blocking(move || {
        db.plugin_store_get_blocking(SETTINGS_NS, SETTINGS_COLLECTION, "caveman_mode")
    })
    .await;
    let level = match raw {
        Ok(Ok(Some(json))) => serde_json::from_str::<serde_json::Value>(&json)
            .ok()
            .and_then(|v| v.get("level").and_then(|l| l.as_str()).map(str::to_string))
            .unwrap_or_else(|| "off".into()),
        _ => "off".into(),
    };
    Json(serde_json::json!({ "level": level }))
}

#[derive(serde::Deserialize)]
struct CavemanBody {
    level: String,
}

/// PUT /api/settings/caveman `{"level":"off|lite|full"}` → 204. Takes effect
/// on each session's next dispatched turn.
async fn set_caveman(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CavemanBody>,
) -> impl IntoResponse {
    if !CAVEMAN_LEVELS.contains(&body.level.as_str()) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "level must be off, lite, or full" })),
        ));
    }
    let db = state.db.clone();
    let value = serde_json::json!({ "level": body.level }).to_string();
    let res = tokio::task::spawn_blocking(move || {
        db.plugin_store_put_blocking(SETTINGS_NS, SETTINGS_COLLECTION, "caveman_mode", &value)
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

/// GET /api/settings/approved-commands → `{"programs":[...]}`, sorted asc.
async fn list_approved(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let db = state.db.clone();
    let rows =
        tokio::task::spawn_blocking(move || db.plugin_store_list_blocking(NS, ALWAYS_COLLECTION))
            .await;

    match rows {
        Ok(Ok(rows)) => {
            // The list is already key-ascending from the DB, but sort again
            // defensively so the contract holds regardless of storage order.
            let mut programs: Vec<String> = rows.into_iter().map(|(key, _)| key).collect();
            programs.sort();
            Ok(Json(serde_json::json!({ "programs": programs })))
        }
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

/// DELETE /api/settings/approved-commands/{program} → 204. Revokes an
/// "always" grant; a missing program is a no-op (still 204).
async fn delete_approved(
    State(state): State<Arc<AppState>>,
    Path(program): Path<String>,
) -> impl IntoResponse {
    let db = state.db.clone();
    let res = tokio::task::spawn_blocking(move || {
        db.plugin_store_delete_blocking(NS, ALWAYS_COLLECTION, &program)
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
