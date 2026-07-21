//! `/api/env-vars/*` — user-defined environment variables injected into the
//! commands agents run (never into the agent process itself; console output
//! is masked — see `plugin::host::exec_impl`). A var is global (`folder_id`
//! NULL) or folder-scoped; a folder var shadows a global one with the same
//! name for sessions in that folder. All routes are JWT-authenticated
//! (`require_auth`).
//!
//! Storage lives in `db::crud::env_vars`; crypto + the in-memory unlock
//! registry live in `service::env_vars`. This module is HTTP glue only:
//! ciphertext/nonce/salt are never returned, and passwords / decrypted
//! values are never logged or broadcast.

use axum::body::Body;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{Request, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

use crate::auth::middleware::{AuthUser, require_auth};
use crate::auth::password::verify_password;
use crate::db::models::NewEnvVar;
use crate::service::env_vars::{decrypt_value, encrypt_value};
use crate::state::AppState;

/// Env var names follow the POSIX identifier shape `^[A-Za-z_][A-Za-z0-9_]*$`.
const NAME_MAX_LEN: usize = 128;
/// A value is capped well above any real secret / config string.
const VALUE_MAX_LEN: usize = 32768;
/// Sanity cap on submitted passwords (matches the askpass surface).
const MAX_PASSWORD_LEN: usize = 1024;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/env-vars", get(list).post(upsert))
        .route("/api/env-vars/{id}", delete(delete_var))
        .route("/api/env-vars/unlock-answer", post(unlock_answer))
        .route("/api/env-vars/lock", post(lock))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

fn auth_user(req: &Request<Body>) -> &AuthUser {
    req.extensions()
        .get::<AuthUser>()
        .expect("auth middleware should inject AuthUser")
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

/// Read + JSON-parse a request body (auth extension must be read first, as
/// this consumes the request).
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
struct EnvVarView {
    id: String,
    name: String,
    encrypted: bool,
    encrypted_by: Option<String>,
    encrypted_by_username: Option<String>,
    folder_id: Option<String>,
    /// Resolved folder name for folder-scoped vars; `null` for global.
    folder_name: Option<String>,
    /// Plaintext only for unencrypted rows; `null` for encrypted rows
    /// (ciphertext/nonce/salt are never surfaced).
    value: Option<String>,
    created_at: String,
    updated_at: String,
}

/// GET /api/env-vars — list every var. Encrypted rows expose metadata only.
async fn list(State(state): State<Arc<AppState>>) -> Response {
    let vars = match state.db.list_env_vars().await {
        Ok(v) => v,
        Err(e) => return internal_err(e),
    };
    // Resolve `encrypted_by` ids to usernames and `folder_id`s to folder
    // names, caching per id so a homogeneous list stays a few lookups.
    let mut usernames: HashMap<String, Option<String>> = HashMap::new();
    let mut folder_names: HashMap<String, Option<String>> = HashMap::new();
    let mut out = Vec::with_capacity(vars.len());
    for v in vars {
        let encrypted_by_username = match &v.encrypted_by {
            Some(uid) => {
                if !usernames.contains_key(uid) {
                    let uname = state
                        .db
                        .get_user(uid)
                        .await
                        .ok()
                        .flatten()
                        .map(|u| u.username);
                    usernames.insert(uid.clone(), uname);
                }
                usernames.get(uid).cloned().flatten()
            }
            None => None,
        };
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
        out.push(EnvVarView {
            id: v.id,
            name: v.name,
            encrypted: v.encrypted,
            encrypted_by: v.encrypted_by,
            encrypted_by_username,
            folder_id: v.folder_id,
            folder_name,
            value: if v.encrypted { None } else { v.value },
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
    #[serde(default)]
    encrypt: bool,
    #[serde(default)]
    password: Option<String>,
    /// Omitted/`null` = global; else the id of the folder the var is
    /// scoped to.
    #[serde(default)]
    folder_id: Option<String>,
}

/// POST /api/env-vars — upsert (by name + scope) a plaintext or encrypted var.
async fn upsert(State(state): State<Arc<AppState>>, request: Request<Body>) -> Response {
    let user_id = auth_user(&request).user_id.clone();
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
    let new = if body.encrypt {
        let password = body.password.unwrap_or_default();
        if password.is_empty() {
            return err(StatusCode::BAD_REQUEST, "password required");
        }
        if password.len() > MAX_PASSWORD_LEN {
            return err(StatusCode::BAD_REQUEST, "password too long");
        }
        // Verify the password against the authenticated user's own hash so a
        // value is never sealed under a mistyped password (it would then be
        // unrecoverable).
        let user = match state.db.get_user(&user_id).await {
            Ok(Some(u)) => u,
            Ok(None) => return err(StatusCode::NOT_FOUND, "user not found"),
            Err(e) => return internal_err(e),
        };
        if !verify_password(&password, &user.password_hash) {
            return err(StatusCode::FORBIDDEN, "wrong password");
        }
        let enc = match encrypt_value(&password, &body.value) {
            Ok(e) => e,
            Err(e) => return internal_err(e),
        };
        NewEnvVar {
            id: uuid::Uuid::new_v4().to_string(),
            name: body.name.clone(),
            value: None,
            ciphertext: Some(enc.ciphertext_b64),
            nonce: Some(enc.nonce_hex),
            kdf_salt: Some(enc.kdf_salt_hex),
            encrypted: true,
            encrypted_by: Some(user_id.clone()),
            folder_id: body.folder_id.clone(),
            created_at: now.clone(),
            updated_at: now,
        }
    } else {
        NewEnvVar {
            id: uuid::Uuid::new_v4().to_string(),
            name: body.name.clone(),
            value: Some(body.value),
            ciphertext: None,
            nonce: None,
            kdf_salt: None,
            encrypted: false,
            encrypted_by: None,
            folder_id: body.folder_id.clone(),
            created_at: now.clone(),
            updated_at: now,
        }
    };

    match state.db.upsert_env_var(new).await {
        Ok(_) => ok(),
        Err(e) => internal_err(e),
    }
}

/// DELETE /api/env-vars/{id} — by id, since a name is only unique per
/// scope. 404 if it doesn't exist.
async fn delete_var(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match state.db.delete_env_var(&id).await {
        Ok(true) => ok(),
        Ok(false) => err(StatusCode::NOT_FOUND, "not found"),
        Err(e) => internal_err(e),
    }
}

#[derive(Deserialize)]
struct UnlockAnswerBody {
    request_id: String,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    cancel: bool,
}

/// POST /api/env-vars/unlock-answer — the unlock dialog submits here. A
/// wrong password leaves the request pending so the dialog can retry.
async fn unlock_answer(State(state): State<Arc<AppState>>, request: Request<Body>) -> Response {
    let caller_id = auth_user(&request).user_id.clone();
    let body: UnlockAnswerBody = match parse_body(request).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let registry = &state.env_unlock;

    if body.cancel {
        // Cancelling is answering too: only the owner may do it. An unknown
        // id (already resolved / timed out) is a no-op, not an error.
        match registry.pending_user(&body.request_id).await {
            None => return ok(),
            Some(uid) if uid != caller_id => return err(StatusCode::FORBIDDEN, "forbidden"),
            Some(_) => {
                registry.resolve(&body.request_id, None).await;
                return ok();
            }
        }
    }

    let Some(pending_uid) = registry.pending_user(&body.request_id).await else {
        return err(StatusCode::GONE, "request no longer pending");
    };
    if pending_uid != caller_id {
        return err(StatusCode::FORBIDDEN, "forbidden");
    }

    let password = body.password.unwrap_or_default();
    if password.len() > MAX_PASSWORD_LEN {
        return err(StatusCode::BAD_REQUEST, "password too long");
    }

    let vars = match state.db.list_env_vars_encrypted_by(&pending_uid).await {
        Ok(v) => v,
        Err(e) => return internal_err(e),
    };

    // Decrypt every var: any failure means the password is wrong. Leave the
    // request pending (no `resolve`) so the dialog can retry. Values are
    // keyed by var id — names are only unique per scope.
    let mut values = HashMap::new();
    for v in vars {
        let (Some(ct), Some(nonce), Some(salt)) = (
            v.ciphertext.as_deref(),
            v.nonce.as_deref(),
            v.kdf_salt.as_deref(),
        ) else {
            return err(StatusCode::FORBIDDEN, "wrong password");
        };
        match decrypt_value(&password, salt, nonce, ct) {
            Some(pt) => {
                values.insert(v.id, pt);
            }
            None => return err(StatusCode::FORBIDDEN, "wrong password"),
        }
    }

    registry.cache_put(&pending_uid, values.clone()).await;
    // Resolve every request waiting on this owner, not just the answered
    // one — the client queues concurrent prompts behind a single dialog, so
    // the later requests would otherwise block until their timeout.
    registry.resolve_all_for_user(&pending_uid, &values).await;
    ok()
}

/// POST /api/env-vars/lock — drop the caller's cached decrypted values.
async fn lock(State(state): State<Arc<AppState>>, request: Request<Body>) -> Response {
    let user_id = auth_user(&request).user_id.clone();
    state.env_unlock.lock_user(&user_id).await;
    ok()
}
