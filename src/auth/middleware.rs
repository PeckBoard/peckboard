use axum::{
    extract::State,
    http::{Request, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;

use super::token::validate_token;
use crate::state::AppState;

/// Authenticated user context injected into request extensions.
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub user_id: String,
    pub role: String,
    pub session_id: String,
}

/// Auth middleware: extracts and validates JWT from Authorization header.
/// Injects `AuthUser` into request extensions on success.
pub async fn require_auth(
    State(state): State<Arc<AppState>>,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let token = match extract_bearer_token(&request) {
        Some(t) => t,
        None => return unauthorized(),
    };

    // Validate JWT signature and expiry
    let claims = match validate_token(&state.jwt_secret, &token) {
        Ok(c) => c,
        Err(_) => return unauthorized(),
    };

    // Check server-side session still exists (revocation check)
    let session_exists = state
        .db
        .get_auth_session(&claims.jti)
        .await
        .ok()
        .flatten()
        .is_some();

    if !session_exists {
        return unauthorized();
    }

    // Update last_used_at
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let _ = state.db.update_auth_session_last_used(&claims.jti, now).await;

    // Inject auth context
    request.extensions_mut().insert(AuthUser {
        user_id: claims.sub,
        role: claims.role,
        session_id: claims.jti,
    });

    next.run(request).await
}

/// Extract bearer token from Authorization header.
fn extract_bearer_token<B>(request: &Request<B>) -> Option<String> {
    let header = request.headers().get(header::AUTHORIZATION)?;
    let value = header.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?;
    Some(token.to_string())
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer realm=\"peckboard\"")],
        axum::Json(serde_json::json!({ "error": "unauthorized" })),
    )
        .into_response()
}

/// Extractor to get the authenticated user from request extensions.
/// Use in handlers that are behind the auth middleware.
pub fn get_auth_user(extensions: &axum::http::Extensions) -> Option<&AuthUser> {
    extensions.get::<AuthUser>()
}

impl AuthUser {
    pub fn is_admin(&self) -> bool {
        self.role == "admin"
    }
}
