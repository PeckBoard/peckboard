use axum::{
    Json, Router,
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, StatusCode, header},
    middleware,
    response::IntoResponse,
    routing::{delete, get, post, put},
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;

use crate::auth::middleware::{AuthUser, require_auth};
use crate::auth::password::{hash_password, verify_password};
use crate::auth::token::{create_token, hash_token};
use crate::db::models::{NewAuthSession, NewUser};
use crate::state::AppState;

/// Minimum allowed password length. Bumped from 8 → 12 to keep a
/// LAN-exposed deployment out of trivial brute-force range when paired
/// with the per-IP login limiter.
const MIN_PASSWORD_LEN: usize = 12;

/// Synthetic Argon2 hash used to keep the user-not-found branch in
/// `login` running for the same wall-clock time as the password-mismatch
/// branch. Hashed once at first call so timing matches a real verify.
/// Using a hash of the literal string "PECKBOARD_LOGIN_TIMING_DECOY"
/// produced at startup means we never compare a real password against
/// it (the hash is well-known, so the only thing it leaks is "no such
/// user" if anyone tries; an attacker who already controls the source
/// has bigger problems).
fn timing_decoy_hash() -> &'static str {
    use std::sync::OnceLock;
    static DECOY: OnceLock<String> = OnceLock::new();
    DECOY.get_or_init(|| {
        hash_password("PECKBOARD_LOGIN_TIMING_DECOY").expect("hashing a constant must not fail")
    })
}

#[derive(Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Deserialize)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

#[derive(Serialize)]
pub struct AuthResponse {
    pub token: String,
    pub user: UserInfo,
}

#[derive(Serialize)]
pub struct UserInfo {
    pub id: String,
    pub username: String,
    pub role: String,
}

#[derive(Serialize)]
pub struct StatusResponse {
    pub has_users: bool,
}

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let public = Router::new()
        .route("/api/auth/status", get(status))
        .route("/api/auth/login", post(login));

    let protected = Router::new()
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/change-password", post(change_password))
        .route("/api/auth/me", get(me))
        .route("/api/auth/sessions", get(list_sessions).delete(revoke_all))
        .route("/api/auth/sessions/{id}", delete(revoke_one))
        .route("/api/users", get(list_users).post(create_user))
        .route("/api/users/{id}", get(get_user).delete(delete_user))
        .route("/api/users/{id}/password", put(admin_set_password))
        .route_layer(middleware::from_fn_with_state(state, require_auth));

    public.merge(protected)
}

/// GET /api/auth/status — kept for compatibility with frontend startup probing.
/// Self-service registration is gone; the bootstrap admin is created server-side
/// on first run, so this always reports `has_users: true`.
async fn status() -> impl IntoResponse {
    Json(StatusResponse { has_users: true })
}

/// POST /api/auth/login
async fn login(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> impl IntoResponse {
    tracing::info!(username = %body.username, "Login attempt");
    // Real per-IP rate limiting. The previous hardcoded 0.0.0.0 bucket
    // meant one attacker could lock out every user on the server.
    let ip = addr.ip();

    let delay = state.login_limiter.check(ip).map_err(|_| {
        (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({"error": "too many login attempts, try again later"})),
        )
    })?;

    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }

    // Look up user. We do NOT short-circuit when the user is missing —
    // instead we verify against a synthetic hash so the user-not-found
    // path takes the same wall time as a wrong-password path. This
    // closes the timing-based username enumeration channel.
    let user = state
        .db
        .get_user_by_username(&body.username)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        })?;

    let (ok, user) = match user {
        Some(u) => {
            let ok = verify_password(&body.password, &u.password_hash);
            (ok, Some(u))
        }
        None => {
            // Burn Argon2 cycles against a fixed decoy hash so the
            // response timing matches a real verify_password call.
            let _ = verify_password(&body.password, timing_decoy_hash());
            (false, None)
        }
    };

    if !ok {
        tracing::warn!(username = %body.username, "Login failed");
        state.login_limiter.record_failure(ip);
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "invalid credentials"})),
        ));
    }

    let user = user.expect("ok=true implies the user existed");

    // Create auth session and token
    let session_id = uuid::Uuid::new_v4().to_string();
    let (token, exp) =
        create_token(&state.jwt_secret, &user.id, &user.role, &session_id).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        })?;

    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.chars().take(256).collect::<String>());
    let ip_address = Some(ip.to_string());

    state
        .db
        .create_auth_session(NewAuthSession {
            id: session_id,
            user_id: user.id.clone(),
            token_hash: hash_token(&token),
            created_at: now_ts,
            expires_at: exp as i64,
            user_agent,
            ip_address,
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        })?;

    state.login_limiter.reset(&ip);

    Ok(Json(AuthResponse {
        token,
        user: UserInfo {
            id: user.id,
            username: user.username,
            role: user.role,
        },
    }))
}

/// POST /api/auth/logout
async fn logout(
    State(state): State<Arc<AppState>>,
    request: axum::http::Request<axum::body::Body>,
) -> impl IntoResponse {
    let auth_user = request
        .extensions()
        .get::<AuthUser>()
        .expect("auth middleware should inject AuthUser");

    tracing::info!(user_id = %auth_user.user_id, "Logging out");
    state
        .db
        .delete_auth_session(&auth_user.session_id)
        .await
        .ok();

    StatusCode::NO_CONTENT
}

/// GET /api/auth/me
async fn me(
    State(state): State<Arc<AppState>>,
    request: axum::http::Request<axum::body::Body>,
) -> impl IntoResponse {
    let auth_user = request
        .extensions()
        .get::<AuthUser>()
        .expect("auth middleware should inject AuthUser");

    let user = state.db.get_user(&auth_user.user_id).await.ok().flatten();

    match user {
        Some(u) => Ok(Json(UserInfo {
            id: u.id,
            username: u.username,
            role: u.role,
        })),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// POST /api/auth/change-password
async fn change_password(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    request: axum::http::Request<axum::body::Body>,
) -> impl IntoResponse {
    let auth_user = request
        .extensions()
        .get::<AuthUser>()
        .expect("auth middleware should inject AuthUser")
        .clone();

    tracing::info!(user_id = %auth_user.user_id, "Changing password");

    // Per-user rate limit: a stolen token can otherwise spam this and
    // lock out the legitimate user before they notice.
    let _delay = state
        .password_change_limiter
        .check(auth_user.user_id.clone())
        .map_err(|_| {
            (
                StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({"error": "too many password changes, try again later"})),
            )
        })?;

    let body: ChangePasswordRequest = {
        let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
            .await
            .map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "invalid body"})),
                )
            })?;
        serde_json::from_slice(&bytes).map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid JSON"})),
            )
        })?
    };

    if body.new_password.len() < MIN_PASSWORD_LEN {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("new password must be at least {MIN_PASSWORD_LEN} characters")
            })),
        ));
    }

    // Get user and verify current password
    let user = state
        .db
        .get_user(&auth_user.user_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        })?
        .ok_or((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "user not found"})),
        ))?;

    if !verify_password(&body.current_password, &user.password_hash) {
        state
            .password_change_limiter
            .record_failure(auth_user.user_id.clone());
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "current password is incorrect"})),
        ));
    }

    // Hash new password and update
    let new_hash = hash_password(&body.new_password).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?;

    let now = chrono::Utc::now().to_rfc3339();
    state
        .db
        .update_user(
            &auth_user.user_id,
            crate::db::models::UpdateUser {
                password_hash: Some(new_hash),
                updated_at: Some(now),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        })?;

    // Re-encrypt this user's encrypted env vars under the new password so
    // they stay unlockable. A var that won't decrypt with the old password
    // (shouldn't happen) is skipped with a warn — name only, never values.
    match state
        .db
        .list_env_vars_encrypted_by(&auth_user.user_id)
        .await
    {
        Ok(vars) => {
            for v in vars {
                let (Some(ct), Some(nonce), Some(salt)) = (
                    v.ciphertext.as_deref(),
                    v.nonce.as_deref(),
                    v.kdf_salt.as_deref(),
                ) else {
                    tracing::warn!(name = %v.name, "env var missing crypto columns; skipping re-encrypt");
                    continue;
                };
                let Some(plaintext) = crate::service::env_vars::decrypt_value(
                    &body.current_password,
                    salt,
                    nonce,
                    ct,
                ) else {
                    tracing::warn!(name = %v.name, "env var failed to decrypt on password change; skipping re-encrypt");
                    continue;
                };
                match crate::service::env_vars::encrypt_value(&body.new_password, &plaintext) {
                    Ok(enc) => {
                        if let Err(e) = state
                            .db
                            .update_env_var_ciphertext(
                                &v.id,
                                &enc.ciphertext_b64,
                                &enc.nonce_hex,
                                &enc.kdf_salt_hex,
                            )
                            .await
                        {
                            tracing::warn!(name = %v.name, error = %e, "failed to store re-encrypted env var");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(name = %v.name, error = %e, "failed to re-encrypt env var");
                    }
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not list env vars to re-encrypt on password change");
        }
    }

    // Revoke all existing auth sessions for this user
    state
        .db
        .delete_auth_sessions_by_user(&auth_user.user_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        })?;

    // Issue fresh token
    let session_id = uuid::Uuid::new_v4().to_string();
    let (token, exp) =
        create_token(&state.jwt_secret, &user.id, &user.role, &session_id).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        })?;

    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.chars().take(256).collect::<String>());
    let ip_address = Some(addr.ip().to_string());

    state
        .db
        .create_auth_session(NewAuthSession {
            id: session_id,
            user_id: user.id.clone(),
            token_hash: hash_token(&token),
            created_at: now_ts,
            expires_at: exp as i64,
            user_agent,
            ip_address,
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        })?;

    state.password_change_limiter.reset(&auth_user.user_id);

    Ok(Json(AuthResponse {
        token,
        user: UserInfo {
            id: user.id,
            username: user.username,
            role: user.role,
        },
    }))
}

/// GET /api/auth/sessions — list caller's auth sessions
async fn list_sessions(
    State(state): State<Arc<AppState>>,
    request: axum::http::Request<axum::body::Body>,
) -> impl IntoResponse {
    let auth_user = request
        .extensions()
        .get::<AuthUser>()
        .expect("auth middleware should inject AuthUser");

    let sessions = state
        .db
        .list_auth_sessions_by_user(&auth_user.user_id)
        .await
        .unwrap_or_default();

    let sessions_json: Vec<serde_json::Value> = sessions
        .iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "created_at": s.created_at,
                "expires_at": s.expires_at,
                "last_used_at": s.last_used_at,
                "user_agent": s.user_agent,
                "ip_address": s.ip_address,
                "current": s.id == auth_user.session_id,
            })
        })
        .collect();

    Json(serde_json::json!({ "sessions": sessions_json }))
}

/// DELETE /api/auth/sessions/:id — revoke one session
async fn revoke_one(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> impl IntoResponse {
    let auth_user = request
        .extensions()
        .get::<AuthUser>()
        .expect("auth middleware should inject AuthUser");

    // Verify the session belongs to the caller (or caller is admin)
    let session = state.db.get_auth_session(&session_id).await.ok().flatten();

    match session {
        Some(s) if s.user_id == auth_user.user_id || auth_user.is_admin() => {
            state.db.delete_auth_session(&session_id).await.ok();
            StatusCode::NO_CONTENT
        }
        Some(_) => StatusCode::FORBIDDEN,
        None => StatusCode::NOT_FOUND,
    }
}

/// DELETE /api/auth/sessions — revoke all except current
async fn revoke_all(
    State(state): State<Arc<AppState>>,
    request: axum::http::Request<axum::body::Body>,
) -> impl IntoResponse {
    let auth_user = request
        .extensions()
        .get::<AuthUser>()
        .expect("auth middleware should inject AuthUser");

    state
        .db
        .delete_auth_sessions_by_user_except(&auth_user.user_id, &auth_user.session_id)
        .await
        .ok();

    StatusCode::NO_CONTENT
}

// ── User Management (admin only) ─────────────────────────────────

#[derive(Deserialize)]
struct CreateUserRequest {
    username: String,
    password: String,
    email: Option<String>,
    role: Option<String>,
}

#[derive(Deserialize)]
struct AdminSetPasswordRequest {
    new_password: String,
}

/// GET /api/users — list all users (admin only)
async fn list_users(
    State(state): State<Arc<AppState>>,
    request: axum::http::Request<axum::body::Body>,
) -> impl IntoResponse {
    let auth_user = request.extensions().get::<AuthUser>().unwrap();
    if !auth_user.is_admin() {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "admin only"})),
        ));
    }

    let users = state.db.list_users().await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?;

    let users_json: Vec<serde_json::Value> = users
        .iter()
        .map(|u| {
            serde_json::json!({
                "id": u.id,
                "username": u.username,
                "email": u.email,
                "role": u.role,
                "created_at": u.created_at,
            })
        })
        .collect();

    Ok(Json(serde_json::json!(users_json)))
}

/// GET /api/users/:id
async fn get_user(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> impl IntoResponse {
    let auth_user = request.extensions().get::<AuthUser>().unwrap();
    if !auth_user.is_admin() && auth_user.user_id != id {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "forbidden"})),
        ));
    }

    let user = state.db.get_user(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?;

    match user {
        Some(u) => Ok(Json(serde_json::json!({
            "id": u.id,
            "username": u.username,
            "email": u.email,
            "role": u.role,
            "created_at": u.created_at,
        }))),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "user not found"})),
        )),
    }
}

/// POST /api/users — create a new user (admin only)
async fn create_user(
    State(state): State<Arc<AppState>>,
    request: axum::http::Request<axum::body::Body>,
) -> impl IntoResponse {
    let auth_user = request.extensions().get::<AuthUser>().unwrap().clone();
    if !auth_user.is_admin() {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "admin only"})),
        ));
    }

    let body: CreateUserRequest = {
        let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
            .await
            .map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "invalid body"})),
                )
            })?;
        serde_json::from_slice(&bytes).map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid JSON"})),
            )
        })?
    };

    if body.username.is_empty() || body.password.len() < MIN_PASSWORD_LEN {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("username required, password min {MIN_PASSWORD_LEN} chars")
            })),
        ));
    }

    let password_hash = hash_password(&body.password).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?;

    let now = chrono::Utc::now().to_rfc3339();
    let user = state
        .db
        .create_user(NewUser {
            id: uuid::Uuid::new_v4().to_string(),
            username: body.username,
            email: body.email,
            password_hash,
            role: body.role.unwrap_or_else(|| "user".into()),
            created_at: now.clone(),
            updated_at: now,
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        })?;

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "id": user.id,
            "username": user.username,
            "email": user.email,
            "role": user.role,
        })),
    ))
}

/// PUT /api/users/:id/password — admin override of a user's password
///
/// Distinct from `POST /api/auth/change-password` (which is self-service
/// and requires the current password). This endpoint is admin-only and
/// does NOT require the target user's current password — that's the
/// whole point: the admin is overriding a forgotten or compromised one.
/// Always revokes the target user's auth sessions so any live tokens
/// belonging to them stop working immediately.
async fn admin_set_password(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> impl IntoResponse {
    let auth_user = request.extensions().get::<AuthUser>().unwrap().clone();
    if !auth_user.is_admin() {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "admin only"})),
        ));
    }

    let body: AdminSetPasswordRequest = {
        let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
            .await
            .map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "invalid body"})),
                )
            })?;
        serde_json::from_slice(&bytes).map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid JSON"})),
            )
        })?
    };

    if body.new_password.len() < MIN_PASSWORD_LEN {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("new password must be at least {MIN_PASSWORD_LEN} characters")
            })),
        ));
    }

    // Confirm the target exists so we can return 404 instead of silently
    // hashing into the void.
    let target = state
        .db
        .get_user(&id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        })?
        .ok_or((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "user not found"})),
        ))?;

    let new_hash = hash_password(&body.new_password).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?;

    let now = chrono::Utc::now().to_rfc3339();
    state
        .db
        .update_user(
            &target.id,
            crate::db::models::UpdateUser {
                password_hash: Some(new_hash),
                updated_at: Some(now),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        })?;

    // Invalidate every auth session belonging to the target. If we're
    // resetting our own password (admin resetting admin), this kicks our
    // own session too — the UI then prompts for fresh login, which is
    // the safe behaviour. For self-service, callers should use
    // `POST /api/auth/change-password` instead so they keep a session.
    state
        .db
        .delete_auth_sessions_by_user(&target.id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        })?;

    tracing::info!(
        admin = %auth_user.user_id,
        target = %target.id,
        "Admin reset password",
    );

    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /api/users/:id — delete a user (admin only)
async fn delete_user(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> impl IntoResponse {
    let auth_user = request.extensions().get::<AuthUser>().unwrap();
    if !auth_user.is_admin() {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "admin only"})),
        ));
    }

    if auth_user.user_id == id {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "cannot delete yourself"})),
        ));
    }

    let deleted = state.db.delete_user(&id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?;

    if !deleted {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "user not found"})),
        ));
    }

    Ok(StatusCode::NO_CONTENT)
}
