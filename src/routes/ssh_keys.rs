//! `/api/ssh-keys/*` — the core SSH key vault: named private keys,
//! encrypted at rest under the server-held vault key (`service::ssh_keys`).
//! Plugins will resolve a key by id through host functions in a later
//! card; this module is REST-only CRUD plus generate/import.
//!
//! Private key material is never serialized into a response — every
//! successful mutation returns [`SshKeyView`], which carries public
//! metadata only. All routes are JWT-authenticated (`require_auth`);
//! mutations (import, generate, rename, delete) are additionally
//! `require_admin` — same reasoning as `routes/env_vars.rs`: a key isn't
//! partitioned per user, so any authenticated user could otherwise rotate
//! or delete a key another user depends on.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, patch, post},
};
use serde::{Deserialize, Serialize};

use crate::auth::middleware::{AuthUser, require_admin, require_auth};
use crate::db::models::{NewSshKey, SshKey};
use crate::service::ssh_keys::{encrypt, generate_keypair, import_private_key};
use crate::state::AppState;

const NAME_MAX_LEN: usize = 128;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    admin_router()
        .merge(user_router())
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// Import, generate, rename, delete — same reasoning as
/// `routes/env_vars.rs`'s `admin_router`: a key has no `user_id`, so any
/// authenticated user could otherwise rotate or delete one shared by
/// everyone.
fn admin_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/ssh-keys", post(import_key))
        .route("/api/ssh-keys/generate", post(generate_key))
        .route("/api/ssh-keys/{id}", patch(rename_key).delete(delete_key))
        .route_layer(middleware::from_fn(require_admin))
}

fn user_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/ssh-keys", get(list))
        .route("/api/ssh-keys/{id}/public", get(get_public))
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

/// Public-facing metadata only — the ciphertext/nonce columns never leave
/// this module.
#[derive(Serialize)]
struct SshKeyView {
    id: String,
    name: String,
    key_type: String,
    public_key: String,
    fingerprint: String,
    has_passphrase: bool,
    created_at: String,
    updated_at: String,
}

impl From<&SshKey> for SshKeyView {
    fn from(k: &SshKey) -> Self {
        SshKeyView {
            id: k.id.clone(),
            name: k.name.clone(),
            key_type: k.key_type.clone(),
            public_key: k.public_key.clone(),
            fingerprint: k.fingerprint.clone(),
            has_passphrase: k.passphrase_ciphertext.is_some(),
            created_at: k.created_at.clone(),
            updated_at: k.updated_at.clone(),
        }
    }
}

/// GET /api/ssh-keys — list every key's metadata.
async fn list(State(state): State<Arc<AppState>>) -> Response {
    match state.db.list_ssh_keys().await {
        Ok(keys) => {
            let views: Vec<SshKeyView> = keys.iter().map(SshKeyView::from).collect();
            (StatusCode::OK, Json(serde_json::json!({ "keys": views }))).into_response()
        }
        Err(e) => internal_err(e),
    }
}

/// GET /api/ssh-keys/{id}/public — the public key, for copying to a
/// server's `authorized_keys`.
async fn get_public(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match state.db.get_ssh_key(&id).await {
        Ok(Some(k)) => (
            StatusCode::OK,
            Json(serde_json::json!({ "public_key": k.public_key })),
        )
            .into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, "not found"),
        Err(e) => internal_err(e),
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= NAME_MAX_LEN
}

#[derive(Deserialize)]
struct ImportBody {
    name: String,
    private_key: String,
    #[serde(default)]
    passphrase: Option<String>,
}

/// POST /api/ssh-keys — import a pasted private key (+ optional
/// passphrase). The key must parse (and, if passphrase-protected,
/// decrypt) before anything is stored.
async fn import_key(
    State(state): State<Arc<AppState>>,
    Extension(auth_user): Extension<AuthUser>,
    Json(body): Json<ImportBody>,
) -> Response {
    let name = body.name.trim().to_string();
    if !valid_name(&name) {
        return err(StatusCode::BAD_REQUEST, "invalid name");
    }
    match state.db.get_ssh_key_by_name(&name).await {
        Ok(Some(_)) => return err(StatusCode::CONFLICT, "name already in use"),
        Ok(None) => {}
        Err(e) => return internal_err(e),
    }

    let passphrase = body.passphrase.filter(|p| !p.is_empty());
    let parsed = match import_private_key(&body.private_key, passphrase.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("invalid private key: {e}"),
            );
        }
    };

    let new = match seal_new_key(
        &state,
        name,
        parsed,
        &body.private_key,
        passphrase.as_deref(),
        &auth_user,
    ) {
        Ok(n) => n,
        Err(e) => return internal_err(e),
    };
    match state.db.insert_ssh_key(new).await {
        Ok(k) => (StatusCode::OK, Json(SshKeyView::from(&k))).into_response(),
        Err(e) => internal_err(e),
    }
}

#[derive(Deserialize)]
struct GenerateBody {
    name: String,
    #[serde(default = "default_key_type")]
    key_type: String,
}

fn default_key_type() -> String {
    "ed25519".to_string()
}

/// POST /api/ssh-keys/generate — generate a fresh keypair and store it.
async fn generate_key(
    State(state): State<Arc<AppState>>,
    Extension(auth_user): Extension<AuthUser>,
    Json(body): Json<GenerateBody>,
) -> Response {
    let name = body.name.trim().to_string();
    if !valid_name(&name) {
        return err(StatusCode::BAD_REQUEST, "invalid name");
    }
    match state.db.get_ssh_key_by_name(&name).await {
        Ok(Some(_)) => return err(StatusCode::CONFLICT, "name already in use"),
        Ok(None) => {}
        Err(e) => return internal_err(e),
    }

    let (private_pem, parsed) = match generate_keypair(&body.key_type) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, &e.to_string()),
    };

    let new = match seal_new_key(&state, name, parsed, &private_pem, None, &auth_user) {
        Ok(n) => n,
        Err(e) => return internal_err(e),
    };
    match state.db.insert_ssh_key(new).await {
        Ok(k) => (StatusCode::OK, Json(SshKeyView::from(&k))).into_response(),
        Err(e) => internal_err(e),
    }
}

/// Encrypt the private key PEM (+ optional passphrase) under the vault key
/// and assemble the row to insert. Shared by import and generate.
fn seal_new_key(
    state: &AppState,
    name: String,
    parsed: crate::service::ssh_keys::ParsedKey,
    private_key_pem: &str,
    passphrase: Option<&str>,
    auth_user: &AuthUser,
) -> anyhow::Result<NewSshKey> {
    let (private_key_ciphertext, private_key_nonce) =
        encrypt(&state.ssh_vault_key, private_key_pem)?;
    let (passphrase_ciphertext, passphrase_nonce) = match passphrase {
        Some(p) => {
            let (ct, nonce) = encrypt(&state.ssh_vault_key, p)?;
            (Some(ct), Some(nonce))
        }
        None => (None, None),
    };
    let now = chrono::Utc::now().to_rfc3339();
    Ok(NewSshKey {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        key_type: parsed.key_type,
        public_key: parsed.public_key,
        fingerprint: parsed.fingerprint,
        private_key_ciphertext,
        private_key_nonce,
        passphrase_ciphertext,
        passphrase_nonce,
        created_at: now.clone(),
        updated_at: now,
        created_by: Some(auth_user.user_id.clone()),
    })
}

#[derive(Deserialize)]
struct RenameBody {
    name: String,
}

/// PATCH /api/ssh-keys/{id} — rename.
async fn rename_key(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<RenameBody>,
) -> Response {
    let name = body.name.trim().to_string();
    if !valid_name(&name) {
        return err(StatusCode::BAD_REQUEST, "invalid name");
    }
    match state.db.get_ssh_key_by_name(&name).await {
        Ok(Some(existing)) if existing.id != id => {
            return err(StatusCode::CONFLICT, "name already in use");
        }
        Ok(_) => {}
        Err(e) => return internal_err(e),
    }
    match state.db.rename_ssh_key(&id, &name).await {
        Ok(true) => ok(),
        Ok(false) => err(StatusCode::NOT_FOUND, "not found"),
        Err(e) => internal_err(e),
    }
}

/// DELETE /api/ssh-keys/{id}.
async fn delete_key(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match state.db.delete_ssh_key(&id).await {
        Ok(true) => ok(),
        Ok(false) => err(StatusCode::NOT_FOUND, "not found"),
        Err(e) => internal_err(e),
    }
}
