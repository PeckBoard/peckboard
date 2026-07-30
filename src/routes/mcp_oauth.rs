//! `/api/mcp-oauth/*` + `GET /oauth/callback` — OAuth sign-in for
//! user-defined remote MCP servers (Settings → MCP Servers).
//!
//! `POST /api/mcp-oauth/start` takes a server draft (saved or not — the
//! token is keyed by the draft's client-generated `id`), resolves the
//! provider's endpoints (registry-configured `oauth` template and/or
//! `.well-known` discovery), registers a client dynamically when the
//! provider supports RFC 7591, and answers with the authorize URL the UI
//! opens in a new tab. The provider redirects the user's browser back to
//! the public `GET /oauth/callback`, which claims the pending login by its
//! one-time `state`, swaps the code for tokens server-side, and stores
//! them. The UI polls `GET /api/mcp-oauth/tokens` until the server id
//! shows up connected.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    middleware,
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
};
use std::sync::Arc;

use crate::auth::middleware::{require_admin, require_auth};
use crate::service::mcp_server::{oauth, user_servers};
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let protected = Router::new()
        .route("/api/mcp-oauth/start", post(start_login))
        .route("/api/mcp-oauth/tokens", get(list_tokens))
        .route("/api/mcp-oauth/claim", post(claim_login))
        .merge(admin_router())
        .route_layer(middleware::from_fn_with_state(state, require_auth));
    // Public on purpose: the provider's redirect arrives without our JWT.
    // The one-time `state` value minted by `start` is the capability that
    // claims the pending login; an unknown state gets a static error page.
    let public = Router::new().route("/oauth/callback", get(callback));
    public.merge(protected)
}

/// Disconnecting revokes a shared MCP server's stored OAuth tokens (no
/// `user_id` on the row), so it's admin-only — same reasoning as
/// `routes/settings.rs`.
///
/// Layers run outer-to-inner on the request, so `require_admin` is appended
/// here and `require_auth` in [`router`] afterwards, which puts `AuthUser`
/// into the extensions before this middleware reads it.
fn admin_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/mcp-oauth/tokens/{server_id}", delete(disconnect))
        .route_layer(middleware::from_fn(require_admin))
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The browser-facing origin of this PeckBoard instance, for building the
/// redirect URI. The SPA's fetch carries an `Origin` header (exactly what
/// the provider must redirect back to); reverse-proxy setups fall back to
/// `X-Forwarded-Proto` + `Host`.
fn request_origin(headers: &HeaderMap) -> Option<String> {
    if let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .filter(|o| o.starts_with("http://") || o.starts_with("https://"))
    {
        return Some(origin.trim_end_matches('/').to_string());
    }
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok())?;
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    Some(format!("{proto}://{host}"))
}

fn bad_request(msg: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": msg.into() })),
    )
        .into_response()
}

#[derive(serde::Deserialize)]
struct StartBody {
    server: user_servers::UserMcpServer,
}

/// POST /api/mcp-oauth/start `{"server": …}` → `{"url": …}`.
///
/// 422 `{"error":"needs_client"}` means the provider offers neither a
/// pre-configured client id nor dynamic registration — the UI should ask
/// the user for a client id/secret from a provider-registered app.
async fn start_login(
    State(_state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<StartBody>,
) -> Response {
    let server = body.server;
    if server.transport != "http" && server.transport != "sse" {
        return bad_request("OAuth sign-in only applies to http/sse servers");
    }
    if !(server.url.starts_with("http://") || server.url.starts_with("https://")) {
        return bad_request("server URL must start with http:// or https://");
    }
    if server.id.trim().is_empty() || server.name.trim().is_empty() {
        return bad_request("server needs an id and a name before signing in");
    }
    let cfg = server.oauth.clone().unwrap_or_default();
    // A redirect broker replaces this instance's own callback: providers
    // that allow neither wildcards nor arbitrary ports in an app's redirect
    // URLs (Slack) can then serve every install from one registration.
    let broker = cfg
        .redirect_broker
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(|b| b.trim_end_matches('/').to_string());
    if let Some(b) = &broker {
        // The broker holds a live authorization code, briefly — no cleartext.
        if !(b.starts_with("https://") || b.starts_with("http://localhost")) {
            return bad_request("the callback broker must be an https:// URL");
        }
    }
    let redirect_uri = match &broker {
        Some(b) => format!("{b}/callback"),
        None => {
            let Some(origin) = request_origin(&headers) else {
                return bad_request("cannot determine this PeckBoard instance's public origin");
            };
            format!("{origin}/oauth/callback")
        }
    };
    let client = oauth::http_client();
    let endpoints = match oauth::discover(&client, &server.url, &cfg).await {
        Ok(e) => e,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": format!("OAuth discovery failed: {e}") })),
            )
                .into_response();
        }
    };

    let (client_id, client_secret) = match cfg.client_id.clone().filter(|c| !c.trim().is_empty()) {
        Some(id) => (
            id,
            cfg.client_secret.clone().filter(|s| !s.trim().is_empty()),
        ),
        None => match &endpoints.registration_url {
            Some(reg) => {
                match oauth::register_client(
                    &client,
                    reg,
                    &redirect_uri,
                    endpoints.scopes.as_deref(),
                )
                .await
                {
                    Ok(pair) => pair,
                    Err(e) => {
                        return (
                            StatusCode::BAD_GATEWAY,
                            Json(serde_json::json!({
                                "error": format!("dynamic client registration failed: {e}"),
                                "redirect_uri": redirect_uri,
                            })),
                        )
                            .into_response();
                    }
                }
            }
            None => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(serde_json::json!({
                        "error": "needs_client",
                        "message": "This provider does not support automatic client registration. \
                                    Create an app with the provider, add the redirect URL below to \
                                    it, then enter its client id (and secret) here.",
                        "redirect_uri": redirect_uri,
                    })),
                )
                    .into_response();
            }
        },
    };

    let url = oauth::begin_login(&endpoints, &server, client_id, client_secret, redirect_uri);
    // `broker` tells the UI to poll /claim instead of waiting for a redirect
    // that will never reach us.
    Json(serde_json::json!({ "url": url, "broker": broker.is_some() })).into_response()
}

#[derive(serde::Deserialize)]
struct ClaimBody {
    server_id: String,
}

/// POST /api/mcp-oauth/claim — the broker flow's stand-in for the browser
/// callback. The UI polls this while the consent tab is open; each call
/// asks the broker once whether it holds a code for this server's in-flight
/// login, and finishes the sign-in the moment it does.
async fn claim_login(State(state): State<Arc<AppState>>, Json(body): Json<ClaimBody>) -> Response {
    let Some((st, broker)) = oauth::pending_broker_login(&body.server_id) else {
        // Nothing in flight — never started, already finished, or expired.
        return Json(serde_json::json!({ "pending": false })).into_response();
    };
    let code = match oauth::claim_code(&oauth::http_client(), &broker, &st).await {
        Ok(Some(c)) => c,
        Ok(None) => return Json(serde_json::json!({ "pending": true })).into_response(),
        Err(e) => {
            // A broker-reported denial ends the attempt; don't poll it forever.
            oauth::take_pending(&st);
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    let Some(login) = oauth::take_pending(&st) else {
        return Json(serde_json::json!({ "pending": false })).into_response();
    };
    match finish_login(&state, &login, &code).await {
        Ok(()) => {
            tracing::info!(
                "mcp oauth: connected server '{}' via redirect broker",
                login.server_name
            );
            Json(serde_json::json!({ "pending": false, "connected": true })).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Swap a claimed code for tokens and store them. Shared by the browser
/// callback and the broker claim — they differ only in how the code got
/// here.
async fn finish_login(
    state: &AppState,
    login: &oauth::PendingLogin,
    code: &str,
) -> anyhow::Result<()> {
    let minted = oauth::exchange_code(&oauth::http_client(), login, code).await?;
    oauth::put_token(
        &state.db,
        oauth::StoredToken {
            server_id: login.server_id.clone(),
            server_name: login.server_name.clone(),
            access_token: minted.access_token,
            refresh_token: minted.refresh_token,
            expires_at_ms: minted.expires_at_ms,
            token_url: login.token_url.clone(),
            client_id: login.client_id.clone(),
            client_secret: login.client_secret.clone(),
            token_field: login.token_field.clone(),
            resource: login.resource.clone(),
            obtained_at_ms: now_ms(),
        },
    )
    .await
}

/// GET /api/mcp-oauth/tokens → `{"tokens": {server_id: {…}}}` — connection
/// status per server id; access/refresh token values never leave the host.
async fn list_tokens(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let tokens = oauth::load_tokens(&state.db).await;
    let view: serde_json::Map<String, serde_json::Value> = tokens
        .into_iter()
        .map(|(id, t)| {
            (
                id,
                serde_json::json!({
                    "server_name": t.server_name,
                    "connected": true,
                    "expires_at_ms": t.expires_at_ms,
                    "obtained_at_ms": t.obtained_at_ms,
                    "has_refresh_token": t.refresh_token.is_some(),
                }),
            )
        })
        .collect();
    Json(serde_json::json!({ "tokens": view }))
}

/// DELETE /api/mcp-oauth/tokens/{server_id} → 204 (404 when not connected).
async fn disconnect(
    State(state): State<Arc<AppState>>,
    Path(server_id): Path<String>,
) -> impl IntoResponse {
    match oauth::remove_token(&state.db, &server_id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
struct CallbackParams {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// Minimal HTML escaping for provider-supplied strings shown on the
/// callback page.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// The tiny standalone page the provider redirect lands on.
fn page(title: &str, detail: &str, ok: bool) -> Html<String> {
    let color = if ok { "#2e7d32" } else { "#c62828" };
    Html(format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title}</title></head>\
         <body style=\"font-family: system-ui, sans-serif; display: flex; \
                       justify-content: center; margin-top: 15vh;\">\
         <div style=\"max-width: 26rem; text-align: center;\">\
         <h2 style=\"color: {color};\">{title}</h2>\
         <p style=\"color: #444;\">{detail}</p>\
         <p style=\"color: #888;\">You can close this tab and return to PeckBoard.</p>\
         </div></body></html>",
        title = esc(title),
        detail = detail,
        color = color,
    ))
}

/// GET /oauth/callback — the provider's browser redirect. Claims the
/// pending login by `state`, exchanges the code, stores the token.
async fn callback(
    State(state): State<Arc<AppState>>,
    Query(params): Query<CallbackParams>,
) -> Html<String> {
    if let Some(err) = params.error.as_deref() {
        if let Some(st) = params.state.as_deref() {
            oauth::take_pending(st);
        }
        let detail = params.error_description.as_deref().unwrap_or("");
        return page(
            "Sign-in failed",
            &format!("The provider reported: <b>{}</b> {}", esc(err), esc(detail)),
            false,
        );
    }
    let Some(st) = params.state.as_deref() else {
        return page(
            "Sign-in failed",
            "The redirect is missing its state parameter.",
            false,
        );
    };
    let Some(login) = oauth::take_pending(st) else {
        return page(
            "Sign-in expired",
            "This sign-in attempt is unknown or older than 15 minutes. \
             Start it again from Settings → MCP Servers.",
            false,
        );
    };
    let Some(code) = params.code.as_deref().filter(|c| !c.is_empty()) else {
        return page(
            "Sign-in failed",
            "The redirect carried no authorization code.",
            false,
        );
    };

    match finish_login(&state, &login, code).await {
        Ok(()) => {
            tracing::info!("mcp oauth: connected server '{}'", login.server_name);
            page(
                "Connected",
                &format!("<b>{}</b> is now signed in.", esc(&login.server_name)),
                true,
            )
        }
        Err(e) => page(
            "Sign-in failed",
            &format!("Sign-in could not be completed: {}", esc(&e.to_string())),
            false,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_origin_prefers_origin_header() {
        let mut h = HeaderMap::new();
        h.insert(header::ORIGIN, "https://pb.example:8443".parse().unwrap());
        h.insert(header::HOST, "internal:80".parse().unwrap());
        assert_eq!(
            request_origin(&h).as_deref(),
            Some("https://pb.example:8443")
        );
    }

    #[test]
    fn request_origin_falls_back_to_forwarded_proto_and_host() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, "pb.example".parse().unwrap());
        h.insert("x-forwarded-proto", "https".parse().unwrap());
        assert_eq!(request_origin(&h).as_deref(), Some("https://pb.example"));

        let mut h2 = HeaderMap::new();
        h2.insert(header::HOST, "10.0.0.5:8080".parse().unwrap());
        assert_eq!(request_origin(&h2).as_deref(), Some("http://10.0.0.5:8080"));

        assert_eq!(request_origin(&HeaderMap::new()), None);
    }

    #[test]
    fn esc_neutralises_html() {
        assert_eq!(esc("<b>&\"'"), "&lt;b&gt;&amp;&quot;&#39;");
    }
}
