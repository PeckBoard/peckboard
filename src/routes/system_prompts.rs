//! `/api/system-prompts/*` — the named system-prompt library.
//!
//! Reusable, named prompts that steer a session's model toward a kind of
//! work (implement / research / debug / review / docs / …). The cost-aware
//! auto-switch applies a matching prompt when it downgrades a session, and
//! the Settings → System Prompts page manages the library.
//!
//! Import downloads a prompt body **server-side** through the same hardened
//! fetch the plugin host uses (`plugin::host::http_fetch_impl`: HTTPS,
//! IP-pinned, no redirects, size-capped) — the browser never fetches, so a
//! malicious URL can't reach the loopback control plane.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{get, post},
};
use std::sync::Arc;

use crate::auth::middleware::require_auth;
use crate::state::AppState;

/// Max characters accepted for a prompt name / body from the API, guarding
/// against absurd inputs (the import path also caps the body via the fetch
/// size limit).
const MAX_NAME_LEN: usize = 200;
const MAX_BODY_LEN: usize = 100_000;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/system-prompts", get(list_prompts).post(create_prompt))
        .route(
            "/api/system-prompts/{id}",
            axum::routing::put(update_prompt).delete(delete_prompt),
        )
        .route("/api/system-prompts/import", post(import_prompt))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

fn err(status: StatusCode, msg: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    (
        status,
        Json(serde_json::json!({ "error": msg.to_string() })),
    )
}

/// GET /api/system-prompts → `{"prompts":[...]}` sorted by name.
async fn list_prompts(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.db.list_system_prompts().await {
        Ok(prompts) => Ok(Json(serde_json::json!({ "prompts": prompts }))),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

#[derive(serde::Deserialize)]
struct CreatePromptBody {
    name: String,
    body: String,
}

/// POST /api/system-prompts `{name, body}` → 201 with the created prompt.
async fn create_prompt(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreatePromptBody>,
) -> impl IntoResponse {
    let name = body.name.trim().to_string();
    if let Err(e) = validate(&name, &body.body) {
        return Err(err(StatusCode::BAD_REQUEST, e));
    }
    if state
        .db
        .get_system_prompt_by_name(&name)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .is_some()
    {
        return Err(err(
            StatusCode::CONFLICT,
            format!("a system prompt named '{name}' already exists"),
        ));
    }
    match state.db.create_system_prompt(&name, &body.body, None).await {
        Ok(p) => Ok((StatusCode::CREATED, Json(serde_json::json!(p)))),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

#[derive(serde::Deserialize)]
struct UpdatePromptBody {
    name: Option<String>,
    body: Option<String>,
}

/// PUT /api/system-prompts/{id} `{name?, body?}` → the updated prompt.
async fn update_prompt(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<UpdatePromptBody>,
) -> impl IntoResponse {
    let name = req.name.map(|s| s.trim().to_string());
    if let Some(n) = &name {
        if n.is_empty() || n.chars().count() > MAX_NAME_LEN {
            return Err(err(StatusCode::BAD_REQUEST, "invalid name"));
        }
        // Editing to collide with a different prompt's name would violate
        // the UNIQUE constraint with an opaque 500 — reject up front.
        if let Some(existing) = state
            .db
            .get_system_prompt_by_name(n)
            .await
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?
            && existing.id != id
        {
            return Err(err(
                StatusCode::CONFLICT,
                format!("a system prompt named '{n}' already exists"),
            ));
        }
    }
    if let Some(b) = &req.body
        && b.chars().count() > MAX_BODY_LEN
    {
        return Err(err(StatusCode::BAD_REQUEST, "body too long"));
    }
    match state
        .db
        .update_system_prompt(&id, name.as_deref(), req.body.as_deref(), None)
        .await
    {
        Ok(Some(p)) => Ok(Json(serde_json::json!(p))),
        Ok(None) => Err(err(StatusCode::NOT_FOUND, "system prompt not found")),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

/// DELETE /api/system-prompts/{id} → 204 (idempotent).
async fn delete_prompt(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.db.delete_system_prompt(&id).await {
        Ok(_) => Ok(StatusCode::NO_CONTENT),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

#[derive(serde::Deserialize)]
struct ImportPromptBody {
    name: String,
    url: String,
}

/// POST /api/system-prompts/import `{name, url}` → the imported/updated
/// prompt. Downloads `url`'s body server-side over the hardened fetch and
/// upserts it by name (a re-import refreshes the body and keeps `source_url`).
async fn import_prompt(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ImportPromptBody>,
) -> impl IntoResponse {
    let name = body.name.trim().to_string();
    let url = body.url.trim().to_string();
    if name.is_empty() || name.chars().count() > MAX_NAME_LEN {
        return Err(err(StatusCode::BAD_REQUEST, "invalid name"));
    }
    // HTTPS only for imports — a prompt is executable-ish config and must
    // come over an authenticated channel.
    if !url.starts_with("https://") {
        return Err(err(StatusCode::BAD_REQUEST, "url must be https"));
    }

    // Reuse the plugin host's hardened fetch (IP-pinned, no redirects,
    // size-capped) on a blocking thread — it spawns its own runtime.
    let fetch_input = serde_json::json!({ "url": url, "method": "GET" }).to_string();
    let raw =
        tokio::task::spawn_blocking(move || crate::plugin::host::http_fetch_impl(&fetch_input))
            .await
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    let parsed: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if let Some(fetch_err) = parsed.get("error").and_then(|v| v.as_str()) {
        return Err(err(
            StatusCode::BAD_GATEWAY,
            format!("fetch failed: {fetch_err}"),
        ));
    }
    let status = parsed.get("status").and_then(|v| v.as_u64()).unwrap_or(0);
    if !(200..300).contains(&status) {
        return Err(err(
            StatusCode::BAD_GATEWAY,
            format!("fetch returned HTTP {status}"),
        ));
    }
    let prompt_body = parsed
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if prompt_body.is_empty() {
        return Err(err(StatusCode::BAD_GATEWAY, "fetched prompt was empty"));
    }
    if prompt_body.chars().count() > MAX_BODY_LEN {
        return Err(err(StatusCode::BAD_REQUEST, "fetched prompt too long"));
    }

    match state
        .db
        .upsert_system_prompt_by_name(&name, &prompt_body, Some(&url))
        .await
    {
        Ok(p) => Ok(Json(serde_json::json!(p))),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

fn validate(name: &str, body: &str) -> Result<(), String> {
    if name.is_empty() || name.chars().count() > MAX_NAME_LEN {
        return Err("name must be 1..=200 characters".into());
    }
    if body.trim().is_empty() {
        return Err("body must not be empty".into());
    }
    if body.chars().count() > MAX_BODY_LEN {
        return Err("body too long".into());
    }
    Ok(())
}
