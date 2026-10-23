//! OAuth 2.1 `client_credentials` for Peckboard's own MCP server.
//!
//! Public (no JWT):
//! - `POST /oauth/token` — swap a client id + secret for a 1-hour bearer.
//! - `GET /.well-known/oauth-authorization-server` (RFC 8414) and
//!   `GET /.well-known/oauth-protected-resource[/mcp]` (RFC 9728) metadata.
//!
//! Admin (JWT + admin): `/api/oauth-clients` CRUD, used by Settings.
//!
//! Token logic and the network policy live in `service::oauth_server`; the
//! `/mcp` side of it in `routes::mcp` (`McpPrincipal::OAuthClient`).

use axum::{
    Json, Router,
    body::Bytes,
    extract::{ConnectInfo, Extension, Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use base64::Engine;
use serde::Deserialize;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, LazyLock};

use crate::auth::middleware::{AuthUser, require_admin, require_auth};
use crate::auth::rate_limit::RateLimiter;
use crate::db::crud::oauth_ts;
use crate::db::models::{NewSession, OAuthAccessToken, OAuthClient};
use crate::service::mcp_server::EXTERNAL_MCP_EXPERT_KIND;
use crate::service::oauth_server::{self, HttpsListener, RequestOrigin};
use crate::service::remote_access::tunnel::Tunnelled;
use crate::state::AppState;

/// Failed client authentications allowed per IP per minute before
/// `/oauth/token` answers 429.
const MAX_FAILED_PER_MINUTE: u32 = 10;

static TOKEN_LIMITER: LazyLock<RateLimiter<IpAddr>> =
    LazyLock::new(|| RateLimiter::new(MAX_FAILED_PER_MINUTE));

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let admin = Router::new()
        .route("/api/oauth-clients", get(list_clients).post(create_client))
        .route("/api/oauth-clients/endpoints", get(endpoints))
        .route("/api/oauth-clients/{id}/rotate-secret", post(rotate_secret))
        .route("/api/oauth-clients/{id}", delete(revoke_client))
        // Layers run outer-to-inner: `require_auth` (added last) puts
        // `AuthUser` into the extensions before `require_admin` reads it.
        .route_layer(middleware::from_fn(require_admin))
        .route_layer(middleware::from_fn_with_state(state, require_auth));
    let public = Router::new()
        .route("/oauth/token", post(token))
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource_metadata),
        )
        // RFC 9728 path-suffixed form, which MCP clients probe first.
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource_metadata),
        );
    public.merge(admin)
}

// ── Token endpoint ─────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct TokenRequest {
    grant_type: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    scope: Option<String>,
}

/// RFC 6749 §5.2 error response.
fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    let mut resp = (
        status,
        Json(serde_json::json!({ "error": error, "error_description": description })),
    )
        .into_response();
    no_store(resp.headers_mut());
    resp
}

fn no_store(headers: &mut HeaderMap) {
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
}

/// `application/x-www-form-urlencoded` body → [`TokenRequest`].
fn parse_form(body: &[u8]) -> Option<TokenRequest> {
    let text = std::str::from_utf8(body).ok()?;
    let mut req = TokenRequest::default();
    for pair in text.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let decode = |s: &str| {
            urlencoding::decode(&s.replace('+', " "))
                .ok()
                .map(|c| c.into_owned())
        };
        let (k, v) = (decode(k)?, decode(v)?);
        let slot = match k.as_str() {
            "grant_type" => &mut req.grant_type,
            "client_id" => &mut req.client_id,
            "client_secret" => &mut req.client_secret,
            "scope" => &mut req.scope,
            _ => continue,
        };
        // RFC 6749 §3.2: request parameters MUST NOT be repeated.
        if slot.is_some() {
            return None;
        }
        *slot = Some(v);
    }
    Some(req)
}

/// `Authorization: Basic base64(urlencode(id):urlencode(secret))`
/// (RFC 6749 §2.3.1). `Some(None)` = a Basic header that doesn't parse.
fn basic_credentials(headers: &HeaderMap) -> Option<Option<(String, String)>> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, rest) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let parsed = (|| {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(rest.trim())
            .ok()?;
        let raw = String::from_utf8(raw).ok()?;
        let (id, secret) = raw.split_once(':')?;
        Some((
            urlencoding::decode(id).ok()?.into_owned(),
            urlencoding::decode(secret).ok()?.into_owned(),
        ))
    })();
    Some(parsed)
}

async fn token(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    tunnelled: Option<Extension<Tunnelled>>,
    https: Option<Extension<HttpsListener>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let origin = RequestOrigin::new(addr, https.is_some(), tunnelled.is_some());
    if !origin.oauth_allowed() {
        return oauth_error(
            StatusCode::FORBIDDEN,
            "invalid_request",
            "the token endpoint is only served over HTTPS or loopback",
        );
    }

    let ip = addr.ip();
    match TOKEN_LIMITER.check(ip) {
        Ok(delay) if !delay.is_zero() => tokio::time::sleep(delay).await,
        Ok(_) => {}
        Err(()) => {
            let mut resp = oauth_error(
                StatusCode::TOO_MANY_REQUESTS,
                "temporarily_unavailable",
                "too many failed client authentications; retry later",
            );
            resp.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
            return resp;
        }
    }

    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.trim_start().starts_with("application/json"));
    let req = if is_json {
        serde_json::from_slice::<TokenRequest>(&body).ok()
    } else {
        parse_form(&body)
    };
    let Some(req) = req else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "malformed request body",
        );
    };

    match req.grant_type.as_deref() {
        None | Some("") => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "grant_type is required",
            );
        }
        Some("client_credentials") => {}
        Some(_) => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "unsupported_grant_type",
                "only client_credentials is supported",
            );
        }
    }

    let basic = basic_credentials(&headers);
    let used_basic = basic.is_some();
    let invalid_client = |description: &str| {
        let mut resp = oauth_error(StatusCode::UNAUTHORIZED, "invalid_client", description);
        if used_basic {
            resp.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"peckboard\""),
            );
        }
        resp
    };
    let (client_id, client_secret) = match basic {
        Some(Some(creds)) => {
            // RFC 6749 §2.3: one authentication method per request.
            if req.client_secret.is_some() {
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "use either HTTP Basic or client_secret in the body, not both",
                );
            }
            if req.client_id.as_deref().is_some_and(|id| id != creds.0) {
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "client_id does not match the Basic credentials",
                );
            }
            creds
        }
        Some(None) => {
            TOKEN_LIMITER.record_failure(ip);
            return invalid_client("malformed Basic credentials");
        }
        None => match (req.client_id.clone(), req.client_secret.clone()) {
            (Some(id), Some(secret)) => (id, secret),
            _ => return invalid_client("client authentication required"),
        },
    };

    if let Some(scope) = req.scope.as_deref()
        && !scope.split_whitespace().all(|s| s == oauth_server::SCOPE)
    {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_scope",
            "the only supported scope is \"mcp\"",
        );
    }

    let client = match state.db.get_oauth_client_by_client_id(&client_id).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("oauth token: client lookup failed: {e}");
            return oauth_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "client lookup failed",
            );
        }
    };
    // Hash-compare even for an unknown client id, so response timing
    // doesn't reveal which ids exist.
    let stored = client
        .as_ref()
        .map(|c| c.secret_hash.clone())
        .unwrap_or_else(|| oauth_server::hash_secret("unknown-client"));
    let secret_ok = oauth_server::verify_secret(&client_secret, &stored);
    let Some(client) = client.filter(|_| secret_ok) else {
        TOKEN_LIMITER.record_failure(ip);
        return invalid_client("unknown client or bad secret");
    };
    TOKEN_LIMITER.reset(&ip);

    let access_token = oauth_server::generate_access_token();
    let now = chrono::Utc::now();
    let row = OAuthAccessToken {
        token_hash: oauth_server::hash_secret(&access_token),
        oauth_client_id: client.id.clone(),
        expires_at: oauth_ts(now + chrono::Duration::seconds(oauth_server::ACCESS_TOKEN_TTL_SECS)),
        created_at: now.to_rfc3339(),
    };
    if let Err(e) = state.db.insert_oauth_access_token(row).await {
        tracing::warn!(client = %client.id, "oauth token: insert failed: {e}");
        return oauth_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "could not issue a token",
        );
    }
    let mut resp = (
        StatusCode::OK,
        Json(serde_json::json!({
            "access_token": access_token,
            "token_type": "Bearer",
            "expires_in": oauth_server::ACCESS_TOKEN_TTL_SECS,
            "scope": oauth_server::SCOPE,
        })),
    )
        .into_response();
    no_store(resp.headers_mut());
    resp
}

// ── Discovery metadata ─────────────────────────────────────────────

async fn authorization_server_metadata(
    State(state): State<Arc<AppState>>,
    https: Option<Extension<HttpsListener>>,
    uri: Uri,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    let base = oauth_server::base_url(&headers, &uri, https.is_some(), state.config.port);
    Json(serde_json::json!({
        "issuer": base,
        "token_endpoint": format!("{base}/oauth/token"),
        "grant_types_supported": ["client_credentials"],
        "token_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post"],
        "scopes_supported": [oauth_server::SCOPE],
    }))
}

async fn protected_resource_metadata(
    State(state): State<Arc<AppState>>,
    https: Option<Extension<HttpsListener>>,
    uri: Uri,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    let base = oauth_server::base_url(&headers, &uri, https.is_some(), state.config.port);
    Json(serde_json::json!({
        "resource": format!("{base}/mcp"),
        "authorization_servers": [base],
        "bearer_methods_supported": ["header"],
        "scopes_supported": [oauth_server::SCOPE],
    }))
}

// ── Admin API ──────────────────────────────────────────────────────

fn api_error(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

fn internal(e: anyhow::Error) -> Response {
    tracing::warn!("oauth clients: {e:#}");
    api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

async fn list_clients(State(state): State<Arc<AppState>>) -> Response {
    match state.db.list_oauth_clients().await {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct CreateClientRequest {
    name: String,
    folder_id: String,
    #[serde(default)]
    project_id: Option<String>,
}

const MAX_NAME_CHARS: usize = 100;

async fn create_client(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Json(body): Json<CreateClientRequest>,
) -> Response {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "name is required");
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return api_error(
            StatusCode::BAD_REQUEST,
            format!("name must be at most {MAX_NAME_CHARS} characters"),
        );
    }
    match state.db.get_folder(&body.folder_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return api_error(StatusCode::BAD_REQUEST, "unknown folder"),
        Err(e) => return internal(e),
    }
    let project_id = body.project_id.filter(|p| !p.is_empty());
    if let Some(pid) = project_id.as_deref() {
        match state.db.get_project(pid).await {
            Ok(Some(p)) if p.folder_id == body.folder_id => {}
            Ok(_) => {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "project is not in the selected folder",
                );
            }
            Err(e) => return internal(e),
        }
    }

    let secret = oauth_server::generate_client_secret();
    let now = chrono::Utc::now().to_rfc3339();
    let session_id = uuid::Uuid::new_v4().to_string();
    let client = OAuthClient {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.clone(),
        client_id: oauth_server::generate_client_id(),
        secret_hash: oauth_server::hash_secret(&secret),
        folder_id: body.folder_id.clone(),
        project_id: project_id.clone(),
        session_id: session_id.clone(),
        created_by: user.user_id.clone(),
        created_at: now.clone(),
        last_used_at: None,
        revoked_at: None,
    };
    // `is_expert` keeps it out of the plain chat lists (the existing
    // mechanism for role sessions); it stays reachable by id from the
    // OAuth clients page, where its ask_user prompts land.
    let session = NewSession {
        id: session_id,
        name: format!("External: {name}"),
        folder_id: body.folder_id,
        project_id,
        created_at: now.clone(),
        last_activity: now,
        is_expert: true,
        expert_kind: Some(EXTERNAL_MCP_EXPERT_KIND.to_string()),
        user_id: Some(user.user_id),
        ..Default::default()
    };
    match state.db.create_oauth_client(client, session).await {
        Ok(view) => (
            StatusCode::CREATED,
            Json(serde_json::json!({ "client": view, "client_secret": secret })),
        )
            .into_response(),
        Err(e) => internal(e),
    }
}

async fn rotate_secret(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let secret = oauth_server::generate_client_secret();
    match state
        .db
        .rotate_oauth_client_secret(&id, &oauth_server::hash_secret(&secret))
        .await
    {
        Ok(true) => Json(serde_json::json!({ "client_secret": secret })).into_response(),
        Ok(false) => api_error(StatusCode::NOT_FOUND, "oauth client not found"),
        Err(e) => internal(e),
    }
}

async fn revoke_client(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match state.db.revoke_oauth_client(&id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => api_error(StatusCode::NOT_FOUND, "oauth client not found"),
        Err(e) => internal(e),
    }
}

/// Where an external system should point: the request's host name on the
/// HTTPS port (OAuth is refused over plain remote HTTP).
async fn endpoints(State(state): State<Arc<AppState>>, uri: Uri, headers: HeaderMap) -> Response {
    let host = oauth_server::request_host(&headers, &uri, state.config.port);
    let hostname = oauth_server::host_without_port(&host);
    let base = format!("https://{hostname}:{}", state.config.https_port);
    Json(serde_json::json!({
        "token_url": format!("{base}/oauth/token"),
        "mcp_url": format!("{base}/mcp"),
        "https_port": state.config.https_port,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_parsing_decodes_and_rejects_repeats() {
        let r =
            parse_form(b"grant_type=client_credentials&client_id=pbc_a%2Bb&scope=mcp+x").unwrap();
        assert_eq!(r.grant_type.as_deref(), Some("client_credentials"));
        assert_eq!(r.client_id.as_deref(), Some("pbc_a+b"));
        assert_eq!(r.scope.as_deref(), Some("mcp x"));
        assert!(parse_form(b"grant_type=a&grant_type=b").is_none());
    }

    #[test]
    fn basic_header_parsing() {
        let mut h = HeaderMap::new();
        let enc = base64::engine::general_purpose::STANDARD.encode("pbc_x:pbcs_y");
        h.insert(
            header::AUTHORIZATION,
            format!("Basic {enc}").parse().unwrap(),
        );
        assert_eq!(
            basic_credentials(&h),
            Some(Some(("pbc_x".to_string(), "pbcs_y".to_string())))
        );
        h.insert(header::AUTHORIZATION, "Basic !!!".parse().unwrap());
        assert_eq!(basic_credentials(&h), Some(None));
        h.insert(header::AUTHORIZATION, "Bearer abc".parse().unwrap());
        assert_eq!(basic_credentials(&h), None);
    }
}
