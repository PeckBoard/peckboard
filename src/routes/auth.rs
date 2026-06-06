use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::auth::middleware::{AuthUser, require_auth};
use crate::auth::password::{hash_password, verify_password};
use crate::auth::token::{create_token, hash_token};
use crate::db::models::{NewAuthSession, NewUser};
use crate::state::AppState;

#[derive(Deserialize)]
pub struct RegisterRequest {
    pub username: String,
    pub password: String,
    pub email: Option<String>,
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
        .route("/api/auth/register", post(register))
        .route("/api/auth/login", post(login));

    let protected = Router::new()
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/change-password", post(change_password))
        .route("/api/auth/me", get(me))
        .route("/api/auth/sessions", get(list_sessions).delete(revoke_all))
        .route("/api/auth/sessions/{id}", delete(revoke_one))
        .route("/api/users", get(list_users).post(create_user))
        .route("/api/users/{id}", get(get_user).delete(delete_user))
        .route_layer(middleware::from_fn_with_state(state, require_auth));

    public.merge(protected)
}

/// GET /api/auth/status — check if any users exist
async fn status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let count = state.db.count_users().await.unwrap_or(0);
    Json(StatusResponse {
        has_users: count > 0,
    })
}

/// POST /api/auth/register — first user registration (disabled once admin exists)
async fn register(
    State(state): State<Arc<AppState>>,
    Json(body): Json<RegisterRequest>,
) -> impl IntoResponse {
    // Only allow registration if no users exist
    let count = state
        .db
        .count_users()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;

    if count > 0 {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "registration disabled — users already exist"})),
        ));
    }

    if body.username.is_empty() || body.password.len() < 8 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "username required and password must be at least 8 characters"})),
        ));
    }

    let password_hash = hash_password(&body.password)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;

    let now = chrono::Utc::now().to_rfc3339();
    let user_id = uuid::Uuid::new_v4().to_string();

    let user = state
        .db
        .create_user(NewUser {
            id: user_id.clone(),
            username: body.username.clone(),
            email: body.email,
            password_hash,
            role: "admin".into(), // First user is always admin
            created_at: now.clone(),
            updated_at: now,
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;

    // Create auth session and token
    let session_id = uuid::Uuid::new_v4().to_string();
    let (token, exp) = create_token(&state.jwt_secret, &user.id, &user.role, &session_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;

    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    state
        .db
        .create_auth_session(NewAuthSession {
            id: session_id,
            user_id: user.id.clone(),
            token_hash: hash_token(&token),
            created_at: now_ts,
            expires_at: exp as i64,
            user_agent: None,
            ip_address: None,
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;

    Ok(Json(AuthResponse {
        token,
        user: UserInfo {
            id: user.id,
            username: user.username,
            role: user.role,
        },
    }))
}

/// POST /api/auth/login
async fn login(
    State(state): State<Arc<AppState>>,
    Json(body): Json<LoginRequest>,
) -> impl IntoResponse {
    // Look up user
    let user = state
        .db
        .get_user_by_username(&body.username)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;

    let user = match user {
        Some(u) => u,
        None => {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "invalid credentials"})),
            ))
        }
    };

    // Verify password
    if !verify_password(&body.password, &user.password_hash) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "invalid credentials"})),
        ));
    }

    // Create auth session and token
    let session_id = uuid::Uuid::new_v4().to_string();
    let (token, exp) = create_token(&state.jwt_secret, &user.id, &user.role, &session_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;

    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    state
        .db
        .create_auth_session(NewAuthSession {
            id: session_id,
            user_id: user.id.clone(),
            token_hash: hash_token(&token),
            created_at: now_ts,
            expires_at: exp as i64,
            user_agent: None,
            ip_address: None,
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;

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
    request: axum::http::Request<axum::body::Body>,
) -> impl IntoResponse {
    let auth_user = request
        .extensions()
        .get::<AuthUser>()
        .expect("auth middleware should inject AuthUser")
        .clone();

    let body: ChangePasswordRequest = {
        let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
            .await
            .map_err(|_| (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "invalid body"}))))?;
        serde_json::from_slice(&bytes)
            .map_err(|_| (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "invalid JSON"}))))?
    };

    if body.new_password.len() < 8 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "new password must be at least 8 characters"})),
        ));
    }

    // Get user and verify current password
    let user = state
        .db
        .get_user(&auth_user.user_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?
        .ok_or((StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "user not found"}))))?;

    if !verify_password(&body.current_password, &user.password_hash) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "current password is incorrect"})),
        ));
    }

    // Hash new password and update
    let new_hash = hash_password(&body.new_password)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;

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
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;

    // Revoke all existing auth sessions
    // TODO: revoke all sessions for this user (need to add this DB method)

    // Issue fresh token
    let session_id = uuid::Uuid::new_v4().to_string();
    let (token, exp) = create_token(&state.jwt_secret, &user.id, &user.role, &session_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;

    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    state
        .db
        .create_auth_session(NewAuthSession {
            id: session_id,
            user_id: user.id.clone(),
            token_hash: hash_token(&token),
            created_at: now_ts,
            expires_at: exp as i64,
            user_agent: None,
            ip_address: None,
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;

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

    // List sessions by user (need DB method)
    // For now, return the current session info
    let session = state
        .db
        .get_auth_session(&auth_user.session_id)
        .await
        .ok()
        .flatten();

    Json(serde_json::json!({
        "sessions": session.map(|s| serde_json::json!({
            "id": s.id,
            "created_at": s.created_at,
            "expires_at": s.expires_at,
            "last_used_at": s.last_used_at,
            "user_agent": s.user_agent,
            "ip_address": s.ip_address,
            "current": true,
        })).into_iter().collect::<Vec<_>>()
    }))
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
    let session = state
        .db
        .get_auth_session(&session_id)
        .await
        .ok()
        .flatten();

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

    // TODO: add DB method to revoke all sessions for a user except one
    // For now, this is a placeholder
    let _ = &auth_user.session_id;
    let _ = state;

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

/// GET /api/users — list all users (admin only)
async fn list_users(
    State(state): State<Arc<AppState>>,
    request: axum::http::Request<axum::body::Body>,
) -> impl IntoResponse {
    let auth_user = request.extensions().get::<AuthUser>().unwrap();
    if !auth_user.is_admin() {
        return Err((StatusCode::FORBIDDEN, Json(serde_json::json!({"error": "admin only"}))));
    }

    let users = state.db.list_users().await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;

    let users_json: Vec<serde_json::Value> = users.iter().map(|u| serde_json::json!({
        "id": u.id,
        "username": u.username,
        "email": u.email,
        "role": u.role,
        "created_at": u.created_at,
    })).collect();

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
        return Err((StatusCode::FORBIDDEN, Json(serde_json::json!({"error": "forbidden"}))));
    }

    let user = state.db.get_user(&id).await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;

    match user {
        Some(u) => Ok(Json(serde_json::json!({
            "id": u.id,
            "username": u.username,
            "email": u.email,
            "role": u.role,
            "created_at": u.created_at,
        }))),
        None => Err((StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "user not found"})))),
    }
}

/// POST /api/users — create a new user (admin only)
async fn create_user(
    State(state): State<Arc<AppState>>,
    request: axum::http::Request<axum::body::Body>,
) -> impl IntoResponse {
    let auth_user = request.extensions().get::<AuthUser>().unwrap().clone();
    if !auth_user.is_admin() {
        return Err((StatusCode::FORBIDDEN, Json(serde_json::json!({"error": "admin only"}))));
    }

    let body: CreateUserRequest = {
        let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024).await
            .map_err(|_| (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "invalid body"}))))?;
        serde_json::from_slice(&bytes)
            .map_err(|_| (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "invalid JSON"}))))?
    };

    if body.username.is_empty() || body.password.len() < 8 {
        return Err((StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "username required, password min 8 chars"}))));
    }

    let password_hash = hash_password(&body.password)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;

    let now = chrono::Utc::now().to_rfc3339();
    let user = state.db.create_user(NewUser {
        id: uuid::Uuid::new_v4().to_string(),
        username: body.username,
        email: body.email,
        password_hash,
        role: body.role.unwrap_or_else(|| "user".into()),
        created_at: now.clone(),
        updated_at: now,
    }).await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;

    Ok((StatusCode::CREATED, Json(serde_json::json!({
        "id": user.id,
        "username": user.username,
        "email": user.email,
        "role": user.role,
    }))))
}

/// DELETE /api/users/:id — delete a user (admin only)
async fn delete_user(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> impl IntoResponse {
    let auth_user = request.extensions().get::<AuthUser>().unwrap();
    if !auth_user.is_admin() {
        return Err((StatusCode::FORBIDDEN, Json(serde_json::json!({"error": "admin only"}))));
    }

    if auth_user.user_id == id {
        return Err((StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "cannot delete yourself"}))));
    }

    let deleted = state.db.delete_user(&id).await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))))?;

    if !deleted {
        return Err((StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "user not found"}))));
    }

    Ok(StatusCode::NO_CONTENT)
}
