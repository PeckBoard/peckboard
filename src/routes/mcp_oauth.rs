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
//!
//! A server's `oauth.redirect_uri` overrides that callback for provider apps
//! registered with a fixed redirect (`http://localhost:8787/callback`): a
//! loopback one gets a temporary listener on its port for the length of
//! the sign-in. `POST /api/mcp-oauth/complete` finishes a login from a
//! pasted redirect URL, for when that loopback redirect lands on a browser
//! that is not on the PeckBoard host.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    middleware,
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
};
use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

use crate::auth::middleware::{require_admin, require_auth};
use crate::service::mcp_server::{oauth, user_servers};
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let protected = Router::new()
        .route("/api/mcp-oauth/tokens", get(list_tokens))
        .merge(admin_router())
        .route_layer(middleware::from_fn_with_state(state, require_auth));
    // Public on purpose: the provider's redirect arrives without our JWT.
    // The one-time `state` value minted by `start` is the capability that
    // claims the pending login; an unknown state gets a static error page.
    let public = Router::new().route("/oauth/callback", get(callback));
    public.merge(protected)
}

/// MCP server definitions and their stored OAuth tokens are shared (no
/// `user_id` on the row), so signing a server in (start + broker claim) or
/// disconnecting it is admin-only — otherwise any user could bind a shared
/// server to their own account, or revoke it, for everyone. Same reasoning
/// as `routes/settings.rs`.
///
/// Layers run outer-to-inner on the request, so `require_admin` is appended
/// here and `require_auth` in [`router`] afterwards, which puts `AuthUser`
/// into the extensions before this middleware reads it.
fn admin_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/mcp-oauth/start", post(start_login))
        .route("/api/mcp-oauth/complete", post(complete_login))
        .route("/api/mcp-oauth/claim", post(claim_login))
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

/// A redirect broker briefly holds a live authorization code, so it must be
/// `https://`, or plain `http://` only to an exact loopback host — the URL is
/// parsed, so `http://localhost.evil.com` / `http://localhost@evil.com` fail.
fn broker_url_ok(broker: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(broker) else {
        return false;
    };
    match url.scheme() {
        "https" => url.host_str().is_some_and(|h| !h.is_empty()),
        // `host_str` is normalised (lowercased domain, bracketed IPv6).
        "http" => matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")),
        _ => false,
    }
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
    State(state): State<Arc<AppState>>,
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
        if !broker_url_ok(b) {
            return bad_request("the callback broker must be an https:// URL");
        }
    }
    // An explicit redirect URI stands in for this instance's own callback
    // when the provider app is registered with a fixed one (a Slack app
    // set up for another client with `http://localhost:8787/callback`).
    let redirect_override = cfg
        .redirect_uri
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string);
    let redirect_uri = match (&broker, redirect_override) {
        (Some(b), _) => format!("{b}/callback"),
        (None, Some(uri)) => {
            // Same rule as the broker: the code travels in cleartext only
            // to this machine.
            if !broker_url_ok(&uri) {
                return bad_request(
                    "the callback URL must be https://, or http:// to localhost / 127.0.0.1 / [::1]",
                );
            }
            if oauth::loopback_redirect(&uri).is_some()
                && !served_by_this_instance(&uri, &headers)
                && let Err(e) = open_loopback_callback(state.clone(), &uri).await
            {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({ "error": e, "redirect_uri": uri })),
                )
                    .into_response();
            }
            uri
        }
        (None, None) => {
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
#[derive(serde::Deserialize)]
struct CompleteBody {
    url: String,
}

/// The callback parameters carried by a pasted redirect URL — absolute, or
/// just the path + query.
fn callback_params_from_url(raw: &str) -> Option<CallbackParams> {
    let raw = raw.trim();
    let url = match reqwest::Url::parse(raw) {
        Ok(u) => u,
        Err(_) => reqwest::Url::parse("http://localhost/")
            .ok()?
            .join(raw)
            .ok()?,
    };
    let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
    Some(CallbackParams {
        code: q.get("code").cloned(),
        state: q.get("state").cloned(),
        error: q.get("error").cloned(),
        error_description: q.get("error_description").cloned(),
    })
}

/// POST /api/mcp-oauth/complete `{"url": …}` — finish a sign-in from the
/// redirect URL copied out of the browser's address bar. The fallback for a
/// loopback redirect URI when the browser is not on the PeckBoard host, so
/// the provider's redirect lands on the *browser's* machine instead.
async fn complete_login(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CompleteBody>,
) -> Response {
    let Some(params) = callback_params_from_url(&body.url) else {
        return bad_request("paste the full address the provider redirected to");
    };
    if let Some(err) = params.error.as_deref() {
        if let Some(st) = params.state.as_deref() {
            oauth::take_pending(st);
        }
        let detail = params.error_description.as_deref().unwrap_or("");
        return bad_request(format!("the provider reported: {err} {detail}").trim_end());
    }
    let (Some(st), Some(code)) = (
        params.state.as_deref(),
        params.code.as_deref().filter(|c| !c.is_empty()),
    ) else {
        return bad_request(
            "that address carries no code and state — copy it after the provider redirects",
        );
    };
    let Some(login) = oauth::take_pending(st) else {
        return bad_request("this sign-in attempt is unknown or expired — start it again");
    };
    match finish_login(&state, &login, code).await {
        Ok(()) => {
            tracing::info!(
                "mcp oauth: connected server '{}' from a pasted redirect",
                login.server_name
            );
            Json(serde_json::json!({ "connected": true })).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Temporary loopback callback listeners
// ---------------------------------------------------------------------------

/// How often an idle temporary listener re-checks whether a login still
/// waits on it.
const LOOPBACK_CHECK: Duration = Duration::from_secs(30);
/// A freshly (re)armed listener is never closed sooner than this, so the
/// login being started right now has time to land in the pending map.
const LOOPBACK_GRACE: Duration = Duration::from_secs(60);

struct Loopback {
    path: String,
    armed: Instant,
}

/// Open temporary listeners, by loopback port.
static LOOPBACKS: LazyLock<std::sync::Mutex<HashMap<u16, Loopback>>> =
    LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));
/// Serialises opening, so two concurrent sign-ins don't race to bind.
static LOOPBACK_OPEN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Whether `uri` points at this very instance (already serving
/// `/oauth/callback`), judged by the loopback origin the UI is using.
fn served_by_this_instance(uri: &str, headers: &HeaderMap) -> bool {
    let Some((_, port, path)) = oauth::loopback_redirect(uri) else {
        return false;
    };
    path == "/oauth/callback"
        && request_origin(headers)
            .and_then(|o| oauth::loopback_redirect(&o))
            .is_some_and(|(_, own, _)| own == port)
}

/// Open (or re-arm) a temporary listener on the loopback port of
/// `redirect_uri`, serving only its path with the same handler as
/// `/oauth/callback`. It closes itself once no unexpired login waits on that
/// port — right after the redirect lands, or when the attempt times out.
pub async fn open_loopback_callback(
    state: Arc<AppState>,
    redirect_uri: &str,
) -> Result<(), String> {
    let Some((host, port, path)) = oauth::loopback_redirect(redirect_uri) else {
        return Err("not a loopback http:// redirect URI".into());
    };
    let _open = LOOPBACK_OPEN.lock().await;
    {
        let mut map = LOOPBACKS.lock().expect("loopback listeners lock");
        if let Some(l) = map.get_mut(&port) {
            if l.path == path {
                l.armed = Instant::now();
                return Ok(());
            }
            return Err(format!(
                "port {port} is already serving a sign-in callback at {}",
                l.path
            ));
        }
    }
    let addrs: Vec<SocketAddr> = match host.as_str() {
        "127.0.0.1" => vec![(Ipv4Addr::LOCALHOST, port).into()],
        "[::1]" => vec![(Ipv6Addr::LOCALHOST, port).into()],
        // `localhost` may resolve to either family in the browser.
        _ => vec![
            (Ipv4Addr::LOCALHOST, port).into(),
            (Ipv6Addr::LOCALHOST, port).into(),
        ],
    };
    let mut listeners = Vec::new();
    let mut last_err = None;
    for addr in addrs {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => listeners.push(l),
            Err(e) => last_err = Some(e),
        }
    }
    if listeners.is_empty() {
        return Err(format!(
            "could not open a temporary callback listener on port {port}: {}",
            last_err.map(|e| e.to_string()).unwrap_or_default()
        ));
    }

    let wake = Arc::new(Notify::new());
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let w = wake.clone();
    let app = Router::new()
        .route(
            &path,
            get(move |st: State<Arc<AppState>>, q: Query<CallbackParams>| {
                let w = w.clone();
                async move {
                    let page = callback(st, q).await;
                    w.notify_one();
                    page
                }
            }),
        )
        .with_state(state);
    for listener in listeners {
        let mut rx = stop_rx.clone();
        let app = app.clone();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = rx.wait_for(|stop| *stop).await;
                })
                .await;
        });
    }
    LOOPBACKS.lock().expect("loopback listeners lock").insert(
        port,
        Loopback {
            path: path.clone(),
            armed: Instant::now(),
        },
    );
    tracing::info!("mcp oauth: temporary callback listener open on {host}:{port}{path}");

    tokio::spawn(async move {
        loop {
            let _ = tokio::time::timeout(LOOPBACK_CHECK, wake.notified()).await;
            {
                let mut map = LOOPBACKS.lock().expect("loopback listeners lock");
                let fresh = map
                    .get(&port)
                    .is_some_and(|l| l.armed.elapsed() < LOOPBACK_GRACE);
                if fresh || oauth::pending_on_loopback_port(port) {
                    continue;
                }
                map.remove(&port);
            }
            let _ = stop_tx.send(true);
            tracing::info!("mcp oauth: temporary callback listener on port {port} closed");
            break;
        }
    });
    Ok(())
}

/// Whether a temporary callback listener is open on `port`.
pub fn loopback_listener_open(port: u16) -> bool {
    LOOPBACKS
        .lock()
        .expect("loopback listeners lock")
        .contains_key(&port)
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

    #[test]
    fn broker_must_be_https_or_exact_loopback() {
        for ok in [
            "https://broker.example",
            "http://localhost:8787",
            "http://127.0.0.1:9000",
            "http://[::1]:9000",
        ] {
            assert!(broker_url_ok(ok), "{ok}");
        }
        for bad in [
            "http://localhost.evil.com",
            "http://localhost@evil.com",
            "http://127.0.0.1.evil.com",
            "http://evil.com",
            "ftp://localhost",
            "not a url",
        ] {
            assert!(!broker_url_ok(bad), "{bad}");
        }
    }

    #[test]
    fn pasted_redirect_url_yields_callback_params() {
        let p = callback_params_from_url(
            "  http://localhost:8787/callback?code=abc&state=s%201&extra=1 ",
        )
        .unwrap();
        assert_eq!(p.code.as_deref(), Some("abc"));
        assert_eq!(p.state.as_deref(), Some("s 1"));
        // Path + query alone also works.
        let p = callback_params_from_url("/callback?error=access_denied&state=x").unwrap();
        assert_eq!(p.error.as_deref(), Some("access_denied"));
        assert_eq!(p.code, None);
    }

    #[test]
    fn only_this_instances_own_callback_skips_the_listener() {
        let mut h = HeaderMap::new();
        h.insert(header::ORIGIN, "http://127.0.0.1:3344".parse().unwrap());
        assert!(served_by_this_instance(
            "http://localhost:3344/oauth/callback",
            &h
        ));
        assert!(!served_by_this_instance(
            "http://localhost:8787/callback",
            &h
        ));
        assert!(!served_by_this_instance(
            "http://localhost:3344/callback",
            &h
        ));
    }
}
