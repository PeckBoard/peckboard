//! `/api/plugins` — built-in plugin catalog and per-plugin settings.
//!
//! Three endpoints:
//!
//! * `GET /api/plugins` — list installed plugins, their permissions,
//!   status, and settings schemas (so the UI can render the form
//!   without a second round trip).
//! * `GET /api/plugins/:plugin_id/settings` — current stored values,
//!   merged with schema defaults. Secret values are redacted (`null`)
//!   and the response carries `has_value` + `masked` flags so the UI
//!   can render "•••••• (set)" vs an empty placeholder.
//! * `PUT /api/plugins/:plugin_id/settings` — bulk update. Validates
//!   every value against the plugin's declared schema and returns 400
//!   with `{field, message}` on the first violation. Built-in plugins
//!   are always enabled today; there is intentionally no enable/disable
//!   route — the user can only adjust the settings they're allowed to
//!   change.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::get,
};

use crate::auth::middleware::require_auth;
use crate::plugin::settings::{apply_updates, redact_for_wire};
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/plugins", get(list_plugins))
        .route(
            "/api/plugins/{plugin_id}/settings",
            get(get_settings).put(update_settings),
        )
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// GET /api/plugins — list installed plugins with their requested
/// permissions, status, and settings schemas, plus the UI panels any
/// loaded WASM plugin contributes. Shape mirrors what the UI consumes
/// directly; see `web/src/components/PluginsSection.tsx`.
///
/// `plugins` is the built-in (statically linked) catalog. `ui_panels` is
/// a flat, validated list of panels declared by loaded WASM plugins —
/// each `{ plugin, id, title, path }` — surfaced alongside the catalog so
/// the Settings UI gets them in the one request it already makes. Panels
/// with an unsafe `path` are dropped by `PluginManager::ui_panels`.
async fn list_plugins(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let entries = state.builtin_plugins.list().await;
    let ui_panels = state.plugins.ui_panels().await;
    Json(serde_json::json!({ "plugins": entries, "ui_panels": ui_panels }))
}

/// GET /api/plugins/:plugin_id/settings — current values, redacted for
/// wire transmission. Always returns 200 with `settings: []` when the
/// plugin exists but has no schema, so the UI can render an empty form
/// without a special-case.
async fn get_settings(
    State(state): State<Arc<AppState>>,
    Path(plugin_id): Path<String>,
) -> impl IntoResponse {
    let Some(schema) = state.builtin_plugins.settings_schema_for(&plugin_id).await else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "unknown plugin" })),
        ));
    };
    let stored = match state.db.list_plugin_settings(&plugin_id).await {
        Ok(s) => s,
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            ));
        }
    };
    let wire = redact_for_wire(&schema, &stored);
    Ok(Json(serde_json::json!({
        "plugin_id": plugin_id,
        "schema": schema,
        "settings": wire,
    })))
}

/// PUT /api/plugins/:plugin_id/settings — bulk update. Body shape:
/// `{"updates": {"base_url": "http://…", "additional_headers": [...]}}`.
/// A field set to `null` (or an empty string/array) deletes the stored
/// row so the schema default takes over.
async fn update_settings(
    State(state): State<Arc<AppState>>,
    Path(plugin_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let Some(schema) = state.builtin_plugins.settings_schema_for(&plugin_id).await else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "unknown plugin" })),
        ));
    };

    let updates = match body.get("updates").and_then(|v| v.as_object()) {
        Some(obj) => obj.clone(),
        None => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "missing or invalid `updates` object" })),
            ));
        }
    };

    let stored = match apply_updates(&state.db, &plugin_id, &schema, &updates).await {
        Ok(s) => s,
        Err(e) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": e.message,
                    "field": e.field,
                })),
            ));
        }
    };

    let wire = redact_for_wire(&schema, &stored);
    Ok(Json(serde_json::json!({
        "plugin_id": plugin_id,
        "schema": schema,
        "settings": wire,
    })))
}
