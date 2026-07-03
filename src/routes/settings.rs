//! `/api/settings/*` — app-level settings surfaced to the authenticated user.
//!
//! Currently just the list of commands the user has granted a persistent
//! "always" approval to `run_command` (via the *Approve always* answer). These
//! live in the native common-tools data store under the `core.common-tools`
//! plugin id, `cli_always` collection (key = program name). The UI reads them
//! so a user can review and revoke standing command grants.

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

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/settings/approved-commands", get(list_approved))
        .route(
            "/api/settings/approved-commands/{program}",
            delete(delete_approved),
        )
        .route_layer(middleware::from_fn_with_state(state, require_auth))
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
