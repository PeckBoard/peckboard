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
    routing::{delete, get, post},
};

use crate::auth::middleware::require_auth;
use crate::plugin::registry;
use crate::plugin::settings::{apply_updates, redact_for_wire};
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/plugins", get(list_plugins))
        .route(
            "/api/plugins/{plugin_id}/settings",
            get(get_settings).put(update_settings),
        )
        .route("/api/plugins/{plugin_id}", delete(uninstall_plugin))
        .route("/api/plugins/{plugin_id}/approval", post(decide_approval))
        .route(
            "/api/plugins/repositories",
            get(list_repositories)
                .post(add_repository)
                .delete(remove_repository),
        )
        .route("/api/plugins/registry", get(list_registry))
        .route("/api/plugins/registry/install", post(install_registry))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// The full set of registry repositories to aggregate: the operator's
/// configured rows plus the optional environment override (which is not
/// removable). Each is `(label, url, removable)`.
async fn all_repositories(state: &AppState) -> anyhow::Result<Vec<(String, String, bool)>> {
    let mut repos: Vec<(String, String, bool)> = Vec::new();
    if let Some((label, url)) = registry::env_repository() {
        repos.push((label, url, false));
    }
    for row in state.db.list_plugin_repositories().await? {
        // De-dupe against the env override by url.
        if repos.iter().any(|(_, u, _)| u == &row.url) {
            continue;
        }
        repos.push((row.label, row.url, true));
    }
    Ok(repos)
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
    // Left-rail entries declared by active WASM plugins (same validation +
    // inert-plugin exclusion as ui_panels).
    let sidebar_items = state.plugins.sidebar_items().await;
    // Loaded WASM plugins and their approval status. The UI uses any with
    // status `pending` to drive the approval prompt; `ui_panels` already
    // excludes panels from unapproved plugins.
    let wasm_plugins = state.plugins.wasm_plugins().await;
    Json(serde_json::json!({
        "plugins": entries,
        "ui_panels": ui_panels,
        "sidebar_items": sidebar_items,
        "wasm_plugins": wasm_plugins,
    }))
}

/// POST /api/plugins/:plugin_id/approval — record an operator's approve or
/// deny decision on a WASM plugin's declared hook set. Body:
/// `{"decision": "approve" | "deny"}`. Approving runs the plugin's
/// deferred `init` and activates its hooks/routes/panels; denying (or any
/// plugin that has never been approved) leaves it inert. The decision is
/// persisted, so it survives restarts as long as the plugin keeps
/// declaring the same hooks.
async fn decide_approval(
    State(state): State<Arc<AppState>>,
    Path(plugin_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let approve = match body.get("decision").and_then(|v| v.as_str()) {
        Some("approve") => true,
        Some("deny") => false,
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "decision must be \"approve\" or \"deny\"" })),
            ));
        }
    };

    match state.plugins.decide(&plugin_id, approve).await {
        Ok(Some(info)) => {
            // Tell every connected client the decision so any open approval
            // prompt updates (and other tabs drop the plugin from theirs).
            state
                .broadcaster
                .broadcast(crate::ws::broadcaster::WsEvent {
                    event_type: "plugin-approval".into(),
                    session_id: String::new(),
                    data: serde_json::json!({ "plugin": info.name, "status": info.status }),
                });
            Ok(Json(serde_json::json!({ "plugin": info })))
        }
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "unknown plugin" })),
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}

/// DELETE /api/plugins/:plugin_id — uninstall an installed WASM plugin.
/// Shuts the plugin down, removes it from the live set, deletes its
/// `.wasm` from disk, and clears its stored approval + settings so a later
/// reinstall starts clean. Built-in plugins live in a separate registry and
/// are never in the WASM set, so this only ever targets installed plugins —
/// an id that doesn't match a loaded WASM plugin returns 404.
async fn uninstall_plugin(
    State(state): State<Arc<AppState>>,
    Path(plugin_id): Path<String>,
) -> impl IntoResponse {
    match state.plugins.uninstall(&plugin_id).await {
        Ok(true) => {
            // Tell every connected client so any open Plugins view drops the
            // plugin (reusing the generic "plugin state changed" signal).
            state
                .broadcaster
                .broadcast(crate::ws::broadcaster::WsEvent {
                    event_type: "plugin-approval".into(),
                    session_id: String::new(),
                    data: serde_json::json!({ "plugin": plugin_id, "status": "removed" }),
                });
            Ok(Json(serde_json::json!({ "removed": plugin_id })))
        }
        Ok(false) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "unknown plugin" })),
        )),
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
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

/// GET /api/plugins/repositories — list configured registry repositories
/// (the environment override, if any, plus the operator's rows).
async fn list_repositories(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match all_repositories(&state).await {
        Ok(repos) => Ok(Json(serde_json::json!({
            "repositories": repos
                .into_iter()
                .map(|(label, url, removable)| serde_json::json!({
                    "label": label, "url": url, "removable": removable,
                }))
                .collect::<Vec<_>>(),
        }))),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}

/// POST /api/plugins/repositories — add a registry repository. Body:
/// `{"repository": "owner/repo" | "https://…/registry.json"}`. The input
/// is resolved to a registry.json URL (a slug → GitHub raw) before storage.
async fn add_repository(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let input = body
        .get("repository")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let (label, url) = match registry::resolve_repo_input(&input) {
        Ok(pair) => pair,
        Err(e) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e.to_string() })),
            ));
        }
    };
    match state.db.add_plugin_repository(&url, &label).await {
        Ok(()) => Ok(Json(
            serde_json::json!({ "repository": { "label": label, "url": url, "removable": true } }),
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}

/// DELETE /api/plugins/repositories — remove a repository by its resolved
/// URL. Body: `{"url": "…"}`. The environment override can't be removed.
async fn remove_repository(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let url = body.get("url").and_then(|v| v.as_str()).unwrap_or("");
    if url.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "missing `url`" })),
        ));
    }
    if registry::env_repository().is_some_and(|(_, u)| u == url) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "the environment registry can't be removed" })),
        ));
    }
    match state.db.remove_plugin_repository(url).await {
        Ok(true) => Ok(Json(serde_json::json!({ "removed": url }))),
        Ok(false) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no such repository" })),
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}

/// GET /api/plugins/registry — aggregate the plugins across every
/// configured repository. Core proxies the fetches (no browser CORS, URLs
/// resolved server-side). Returns each repository with a reachability
/// status and the merged plugin list, every entry tagged with its source
/// repository and whether it's already installed. A single repository
/// being down doesn't fail the whole response — it's reported per-repo.
async fn list_registry(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let repos = match all_repositories(&state).await {
        Ok(r) => r,
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            ));
        }
    };

    // Installed wasm plugins, keyed name → version, so each registry entry can
    // be tagged with the installed version and whether a newer one is on offer.
    let installed: std::collections::HashMap<String, String> = state
        .plugins
        .wasm_plugins()
        .await
        .into_iter()
        .map(|p| (p.name, p.version))
        .collect();
    let running = registry::peckboard_version();

    let client = reqwest::Client::new();
    let mut repo_statuses = Vec::new();
    let mut plugins = Vec::new();
    for (label, url, removable) in repos {
        match registry::fetch_index(&client, &url).await {
            Ok(index) => {
                repo_statuses.push(serde_json::json!({
                    "label": label, "url": url, "removable": removable, "ok": true,
                }));
                for e in index.plugins {
                    let installed_version = installed.get(&e.id).cloned();
                    let compatible = registry::is_compatible(running, e.min_peckboard.as_deref());
                    // An upgrade is offered only when installed AND the index
                    // version is strictly newer than what's loaded.
                    let upgrade_available = installed_version
                        .as_deref()
                        .map(|iv| registry::is_newer(&e.version, iv))
                        .unwrap_or(false);
                    plugins.push(serde_json::json!({
                        "id": e.id,
                        "name": e.name,
                        "description": e.description,
                        "author": e.author,
                        "homepage": e.homepage,
                        "version": e.version,
                        "hooks": e.hooks,
                        "repository": url,
                        "repository_label": label,
                        "installed": installed_version.is_some(),
                        "installed_version": installed_version,
                        "min_peckboard": e.min_peckboard,
                        "compatible": compatible,
                        "upgrade_available": upgrade_available,
                    }));
                }
            }
            Err(e) => {
                repo_statuses.push(serde_json::json!({
                    "label": label, "url": url, "removable": removable,
                    "ok": false, "error": e.to_string(),
                }));
            }
        }
    }

    Ok(Json(serde_json::json!({
        "repositories": repo_statuses,
        "plugins": plugins,
        "peckboard_version": running,
    })))
}

/// POST /api/plugins/registry/install — install a registry plugin. Body:
/// `{"id": "api", "repository": "<resolved url>"}`. Core fetches that
/// repository's index (or searches all repositories when `repository` is
/// omitted), looks the id up there (never trusting a client-supplied
/// download URL), downloads the `.wasm`, verifies its SHA-256, then loads
/// it **inert** — it surfaces in the approval prompt and runs nothing
/// until the operator approves its hooks.
async fn install_registry(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let id = match body.get("id").and_then(|v| v.as_str()) {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "missing or empty `id`" })),
            ));
        }
    };
    let repository = body
        .get("repository")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let all = match all_repositories(&state).await {
        Ok(r) => r,
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            ));
        }
    };
    // Restrict to the named repository when given; otherwise search all.
    let candidates: Vec<(String, String, bool)> = match &repository {
        Some(url) => all.into_iter().filter(|(_, u, _)| u == url).collect(),
        None => all,
    };
    if candidates.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "unknown repository" })),
        ));
    }

    let client = reqwest::Client::new();
    // Find the entry for `id` in the candidate repositories.
    let mut found: Option<registry::RegistryEntry> = None;
    for (_, url, _) in &candidates {
        if let Ok(index) = registry::fetch_index(&client, url).await
            && let Some(entry) = index.plugins.into_iter().find(|e| e.id == id)
        {
            found = Some(entry);
            break;
        }
    }
    let Some(entry) = found else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("no plugin '{id}' in the registry") })),
        ));
    };

    // Refuse an install/upgrade the running Peckboard can't support. The UI
    // already greys the button out; this is the matching server-side guard so
    // a direct API call can't bypass the floor.
    let running = registry::peckboard_version();
    if !registry::is_compatible(running, entry.min_peckboard.as_deref()) {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!(
                    "plugin '{id}' requires Peckboard >= {} (running {running})",
                    entry.min_peckboard.as_deref().unwrap_or("?"),
                ),
            })),
        ));
    }

    // Download + integrity-check against the index's sha256.
    let bytes = match registry::download_and_verify(&client, &entry.url, &entry.sha256).await {
        Ok(bytes) => bytes,
        Err(e) => {
            return Err((
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": format!("download failed: {e}") })),
            ));
        }
    };

    match state.plugins.install(&entry.id, &bytes).await {
        Ok(info) => {
            state
                .broadcaster
                .broadcast(crate::ws::broadcaster::WsEvent {
                    event_type: "plugin-approval".into(),
                    session_id: String::new(),
                    data: serde_json::json!({ "plugin": info.name, "status": info.status }),
                });
            Ok(Json(serde_json::json!({ "plugin": info })))
        }
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}
