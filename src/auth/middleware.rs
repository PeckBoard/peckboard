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
    let _ = state
        .db
        .update_auth_session_last_used(&claims.jti, now)
        .await;

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

/// Admin-only middleware. MUST be layered AFTER `require_auth` so the
/// `AuthUser` extension is already present. Sessions, cards, and other
/// per-tenant data are not partitioned by user id in the DB (single
/// admin is the design), so the route layer enforces "admin only" to
/// keep an admin-created non-admin user from reading or modifying
/// admin-owned state by guessing UUIDs.
pub async fn require_admin(request: Request<axum::body::Body>, next: Next) -> Response {
    let is_admin = request
        .extensions()
        .get::<AuthUser>()
        .map(|u| u.is_admin())
        .unwrap_or(false);

    if !is_admin {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({ "error": "admin only" })),
        )
            .into_response();
    }

    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::token::{create_token, generate_jwt_secret};
    use crate::config::Config;
    use crate::db::Db;
    use crate::db::models::{NewAuthSession, NewUser};
    use axum::{Router, body::Body, middleware, routing::get};
    use tower::ServiceExt;

    fn test_state(dir: &std::path::Path) -> Arc<AppState> {
        let provider_registry = Arc::new(crate::provider::registry::ProviderRegistry::new());
        Arc::new(AppState {
            config: Config {
                port: 0,
                https_port: 0,
                host: "127.0.0.1".into(),
                data_dir: dir.to_path_buf(),
                mdns: false,
                keep_alive_hours: 0,
            },
            db: Db::in_memory().unwrap(),
            plugins: Arc::new(crate::plugin::manager::PluginManager::empty()),
            builtin_plugins: Arc::new(crate::plugin::builtin::BuiltinPluginRegistry::new()),
            jwt_secret: generate_jwt_secret(),
            login_limiter: crate::auth::rate_limit::RateLimiter::new(100),
            password_change_limiter: crate::auth::rate_limit::RateLimiter::new(100),
            broadcaster: crate::ws::broadcaster::Broadcaster::new(),
            provider_registry: provider_registry.clone(),
            session_manager: crate::provider::manager::SessionManager::new(provider_registry),
            repeating_task_manager: crate::repeating::RepeatingTaskManager::new(),
            run_auditor: crate::repeating::RunAuditor::new(),
            mcp_tokens: crate::service::mcp_server::McpTokenRegistry::new(),
            push_service: crate::service::push::PushService::new(dir),
        })
    }

    /// Seed a user + auth session and return a valid bearer token for it.
    async fn seed_authenticated_user(state: &Arc<AppState>, role: &str) -> String {
        let now_str = chrono::Utc::now().to_rfc3339();
        let user_id = "u1".to_string();
        state
            .db
            .create_user(NewUser {
                id: user_id.clone(),
                username: "alice".into(),
                email: None,
                password_hash: "x".into(),
                role: role.into(),
                created_at: now_str.clone(),
                updated_at: now_str,
            })
            .await
            .unwrap();

        let session_id = "as1".to_string();
        state
            .db
            .create_auth_session(NewAuthSession {
                id: session_id.clone(),
                user_id: user_id.clone(),
                token_hash: "deadbeef".into(),
                created_at: 0,
                expires_at: i64::MAX,
                user_agent: None,
                ip_address: None,
            })
            .await
            .unwrap();

        let (token, _) = create_token(&state.jwt_secret, &user_id, role, &session_id).unwrap();
        token
    }

    fn protected_app(state: Arc<AppState>) -> Router {
        async fn whoami(axum::Extension(user): axum::Extension<AuthUser>) -> String {
            format!("{}:{}:{}", user.user_id, user.role, user.session_id)
        }
        Router::new()
            .route("/protected", get(whoami))
            .layer(middleware::from_fn_with_state(state, require_auth))
    }

    fn request(token: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().uri("/protected");
        if let Some(t) = token {
            builder = builder.header(header::AUTHORIZATION, t);
        }
        builder.body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn missing_authorization_header_is_unauthorized() {
        let dir = tempfile::tempdir().unwrap();
        let app = protected_app(test_state(dir.path()));

        let response = app.oneshot(request(None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response
                .headers()
                .get(header::WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer realm=\"peckboard\"")
        );
    }

    #[tokio::test]
    async fn malformed_authorization_header_is_unauthorized() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "admin").await;

        // Valid token but wrong scheme / missing "Bearer " prefix.
        for value in [token.as_str(), "Basic abc123", "Bearer"] {
            let response = protected_app(state.clone())
                .oneshot(request(Some(value)))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "header {value:?} must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn garbage_token_is_unauthorized() {
        let dir = tempfile::tempdir().unwrap();
        let app = protected_app(test_state(dir.path()));

        let response = app
            .oneshot(request(Some("Bearer not.a.jwt")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn token_signed_with_wrong_secret_is_unauthorized() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        seed_authenticated_user(&state, "admin").await;

        let (forged, _) = create_token(&generate_jwt_secret(), "u1", "admin", "as1").unwrap();
        let response = protected_app(state)
            .oneshot(request(Some(&format!("Bearer {forged}"))))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn revoked_session_is_unauthorized() {
        // A structurally valid token whose server-side session is gone
        // (logout / reset) must be rejected.
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "admin").await;
        state.db.delete_auth_session("as1").await.unwrap();

        let response = protected_app(state)
            .oneshot(request(Some(&format!("Bearer {token}"))))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn valid_token_passes_and_injects_auth_user() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "admin").await;

        let response = protected_app(state)
            .oneshot(request(Some(&format!("Bearer {token}"))))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"u1:admin:as1");
    }

    fn admin_app(user: Option<AuthUser>) -> Router {
        // Outer layer injects (or omits) the AuthUser the way
        // `require_auth` would, inner layer is the one under test.
        Router::new()
            .route("/admin", get(|| async { "ok" }))
            .layer(middleware::from_fn(require_admin))
            .layer(middleware::from_fn(
                move |mut req: Request<axum::body::Body>, next: Next| {
                    let user = user.clone();
                    async move {
                        if let Some(u) = user {
                            req.extensions_mut().insert(u);
                        }
                        next.run(req).await
                    }
                },
            ))
    }

    #[tokio::test]
    async fn require_admin_allows_admin_role() {
        let app = admin_app(Some(AuthUser {
            user_id: "u1".into(),
            role: "admin".into(),
            session_id: "s1".into(),
        }));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn require_admin_rejects_non_admin_and_missing_user() {
        for user in [
            Some(AuthUser {
                user_id: "u1".into(),
                role: "member".into(),
                session_id: "s1".into(),
            }),
            None,
        ] {
            let response = admin_app(user)
                .oneshot(
                    Request::builder()
                        .uri("/admin")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
    }
}
