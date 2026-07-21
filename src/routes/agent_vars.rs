//! `/api/agent-vars/*` — user-managed side of agent variables: plain
//! name/value state agents read AND write through the MCP tools
//! (`list_variables` / `set_variable` / `delete_variable`). A var is global
//! (`folder_id` NULL) or folder-scoped; a folder var shadows a global one
//! with the same name for sessions in that folder. Values are agent-readable
//! by design — no encryption, no masking — so they must not hold secrets
//! (that's what env vars are for). All routes are JWT-authenticated
//! (`require_auth`).

use axum::body::Body;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{Request, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{delete, get},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

use crate::auth::middleware::require_auth;
use crate::db::models::NewAgentVar;
use crate::state::AppState;

/// Var names follow the POSIX identifier shape `^[A-Za-z_][A-Za-z0-9_]*$`
/// (same rule as env vars — one grammar to remember).
const NAME_MAX_LEN: usize = 128;
/// Same generous cap as env var values.
const VALUE_MAX_LEN: usize = 32768;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/agent-vars", get(list).post(upsert))
        .route("/api/agent-vars/{id}", delete(delete_var))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}

fn internal_err(e: impl std::fmt::Display) -> Response {
    err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
}

fn ok() -> Response {
    (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response()
}

async fn parse_body<T: serde::de::DeserializeOwned>(request: Request<Body>) -> Result<T, Response> {
    let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .map_err(|_| err(StatusCode::BAD_REQUEST, "invalid body"))?;
    serde_json::from_slice(&bytes).map_err(|_| err(StatusCode::BAD_REQUEST, "invalid JSON"))
}

/// `^[A-Za-z_][A-Za-z0-9_]*$`, length ≤ [`NAME_MAX_LEN`].
fn valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > NAME_MAX_LEN {
        return false;
    }
    let first = name.as_bytes()[0];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

#[derive(Serialize)]
struct AgentVarView {
    id: String,
    name: String,
    value: String,
    folder_id: Option<String>,
    /// Resolved folder name for folder-scoped vars; `null` for global.
    folder_name: Option<String>,
    created_at: String,
    updated_at: String,
}

/// GET /api/agent-vars — every var across all scopes.
async fn list(State(state): State<Arc<AppState>>) -> Response {
    let vars = match state.db.list_agent_vars().await {
        Ok(v) => v,
        Err(e) => return internal_err(e),
    };
    // Resolve folder ids to names, caching per id.
    let mut folder_names: HashMap<String, Option<String>> = HashMap::new();
    let mut out = Vec::with_capacity(vars.len());
    for v in vars {
        let folder_name = match &v.folder_id {
            Some(fid) => {
                if !folder_names.contains_key(fid) {
                    let fname = state
                        .db
                        .get_folder(fid)
                        .await
                        .ok()
                        .flatten()
                        .map(|f| f.name);
                    folder_names.insert(fid.clone(), fname);
                }
                folder_names.get(fid).cloned().flatten()
            }
            None => None,
        };
        out.push(AgentVarView {
            id: v.id,
            name: v.name,
            value: v.value,
            folder_id: v.folder_id,
            folder_name,
            created_at: v.created_at,
            updated_at: v.updated_at,
        });
    }
    (StatusCode::OK, Json(serde_json::json!({ "vars": out }))).into_response()
}

#[derive(Deserialize)]
struct UpsertBody {
    name: String,
    #[serde(default)]
    value: String,
    /// Omitted/`null` = global; else the id of the folder the var is
    /// scoped to.
    #[serde(default)]
    folder_id: Option<String>,
}

/// POST /api/agent-vars — upsert by (name, scope).
async fn upsert(State(state): State<Arc<AppState>>, request: Request<Body>) -> Response {
    let body: UpsertBody = match parse_body(request).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };

    if !valid_name(&body.name) {
        return err(StatusCode::BAD_REQUEST, "invalid name");
    }
    if body.value.len() > VALUE_MAX_LEN {
        return err(StatusCode::BAD_REQUEST, "value too long");
    }
    // A folder-scoped var must reference an existing folder.
    if let Some(fid) = &body.folder_id {
        match state.db.get_folder(fid).await {
            Ok(Some(_)) => {}
            Ok(None) => return err(StatusCode::BAD_REQUEST, "unknown folder"),
            Err(e) => return internal_err(e),
        }
    }

    let now = chrono::Utc::now().to_rfc3339();
    let new = NewAgentVar {
        id: uuid::Uuid::new_v4().to_string(),
        name: body.name,
        value: body.value,
        folder_id: body.folder_id,
        created_at: now.clone(),
        updated_at: now,
    };
    match state.db.upsert_agent_var(new).await {
        Ok(_) => ok(),
        Err(e) => internal_err(e),
    }
}

/// DELETE /api/agent-vars/{id} — 404 if it doesn't exist.
async fn delete_var(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match state.db.delete_agent_var_by_id(&id).await {
        Ok(true) => ok(),
        Ok(false) => err(StatusCode::NOT_FOUND, "not found"),
        Err(e) => internal_err(e),
    }
}
