//! Generic OAuth 2.1 sign-in for user-defined remote MCP servers
//! (Settings → MCP Servers, `auth: "oauth"`).
//!
//! The flow follows the MCP authorization spec: RFC 9728 protected-resource
//! metadata on the MCP URL names the authorization server, RFC 8414 / OIDC
//! metadata on that server names the authorize/token/registration
//! endpoints, RFC 7591 dynamic client registration mints a client id when
//! the server supports it, and the login itself is a standard
//! authorization-code + PKCE (S256) redirect back to our own
//! `GET /oauth/callback`. Registry entries can pre-fill or override any
//! piece via [`McpOauthConfig`] — providers without discovery/DCR (Slack)
//! ship static endpoints and take a user-created client id/secret instead.
//!
//! Tokens are stored per server **id** in the core-settings plugin store
//! (key [`MCP_OAUTH_TOKENS_KEY`]) — same trust boundary as the manually
//! pasted `Authorization` header values stored right next to them — and
//! [`bearer_for_server`] resolves (auto-refreshing near expiry, mirroring
//! `provider::claude::token_refresh`) the header injected at dispatch by
//! `user_servers::entries_for_provider_with_oauth`.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::user_servers::{McpOauthConfig, UserMcpServer};
use crate::db::Db;
use crate::routes::settings::{SETTINGS_COLLECTION, SETTINGS_NS};

/// Plugin-store key under `SETTINGS_NS`/`SETTINGS_COLLECTION`: a JSON map
/// of server id → [`StoredToken`].
pub const MCP_OAUTH_TOKENS_KEY: &str = "mcp_oauth_tokens";

/// Refresh when the access token is within this margin of expiry.
const REFRESH_MARGIN_MS: i64 = 5 * 60 * 1000;

/// A started login is abandoned after this long.
const PENDING_TTL_MS: i64 = 15 * 60 * 1000;

/// Per-request budget for discovery/registration/token calls.
const HTTP_TIMEOUT_SECS: u64 = 15;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A reqwest client for the short OAuth conversations.
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// PKCE / small string helpers
// ---------------------------------------------------------------------------

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// 32 random bytes, base64url — the PKCE verifier / state alphabet.
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    b64url(&bytes)
}

/// PKCE S256 challenge: `base64url(sha256(verifier))`.
fn challenge_for(verifier: &str) -> String {
    b64url(&Sha256::digest(verifier.as_bytes()))
}

fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Split an http(s) URL into `(origin, path)`; query/fragment dropped,
/// `path` is `""` for a bare origin, otherwise starts with `/`.
fn origin_and_path(url: &str) -> Option<(String, String)> {
    let scheme_end = url.find("://")?;
    let rest = &url[scheme_end + 3..];
    let cut = rest.find(['?', '#']).unwrap_or(rest.len());
    let rest = &rest[..cut];
    match rest.find('/') {
        Some(slash) => {
            let origin = format!("{}{}", &url[..scheme_end + 3], &rest[..slash]);
            let path = rest[slash..].trim_end_matches('/').to_string();
            Some((origin, path))
        }
        None => Some((format!("{}{}", &url[..scheme_end + 3], rest), String::new())),
    }
}

// ---------------------------------------------------------------------------
// Discovery (RFC 9728 + RFC 8414 / OIDC)
// ---------------------------------------------------------------------------

/// Everything needed to run and complete a login against one server.
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoints {
    pub authorize_url: String,
    pub token_url: String,
    pub registration_url: Option<String>,
    /// RFC 8707 `resource` value, when RFC 9728 metadata advertised one.
    pub resource: Option<String>,
    /// Space-separated scopes to request (config wins over metadata).
    pub scopes: Option<String>,
    /// Query parameter carrying the scopes (default `scope`; Slack's user
    /// flow wants `user_scope`).
    pub scope_param: Option<String>,
    /// Extra authorize-request query params (SSO/team pinning hints),
    /// pre-filtered against the reserved set.
    pub auth_params: Vec<(String, String)>,
}

/// `cfg.auth_params` as ready-to-append pairs — blank and reserved names
/// dropped (validation rejects them upstream; this is defence in depth).
fn extra_auth_params(cfg: &McpOauthConfig) -> Vec<(String, String)> {
    cfg.auth_params
        .iter()
        .filter(|kv| {
            let k = kv.key.trim();
            !k.is_empty() && !super::user_servers::RESERVED_AUTH_PARAMS.contains(&k)
        })
        .map(|kv| (kv.key.trim().to_string(), kv.value.clone()))
        .collect()
}

async fn fetch_json(client: &reqwest::Client, url: &str) -> Option<serde_json::Value> {
    let resp = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json().await.ok()
}

/// RFC 9728: the protected-resource metadata candidates for an MCP URL —
/// path-aware form first, then the origin root.
fn prm_candidates(origin: &str, path: &str) -> Vec<String> {
    let wk = "/.well-known/oauth-protected-resource";
    let mut v = Vec::new();
    if !path.is_empty() {
        v.push(format!("{origin}{wk}{path}"));
    }
    v.push(format!("{origin}{wk}"));
    v
}

/// RFC 8414 (+ OIDC fallback) metadata candidates for an issuer URL.
fn as_metadata_candidates(origin: &str, path: &str) -> Vec<String> {
    let rfc = "/.well-known/oauth-authorization-server";
    let oidc = "/.well-known/openid-configuration";
    let mut v = Vec::new();
    if !path.is_empty() {
        v.push(format!("{origin}{rfc}{path}"));
        v.push(format!("{origin}{oidc}{path}"));
        v.push(format!("{origin}{path}{oidc}"));
    }
    v.push(format!("{origin}{rfc}"));
    v.push(format!("{origin}{oidc}"));
    v
}

/// Resolve the endpoints for `mcp_url`, honouring `cfg` overrides field by
/// field. Static config short-circuits discovery entirely when it names
/// both the authorize and token URLs.
pub async fn discover(
    client: &reqwest::Client,
    mcp_url: &str,
    cfg: &McpOauthConfig,
) -> anyhow::Result<Endpoints> {
    if let (Some(authorize), Some(token)) = (&cfg.authorize_url, &cfg.token_url) {
        return Ok(Endpoints {
            authorize_url: authorize.clone(),
            token_url: token.clone(),
            registration_url: cfg.registration_url.clone(),
            resource: None,
            scopes: cfg.scopes.clone(),
            scope_param: cfg.scope_param.clone(),
            auth_params: extra_auth_params(cfg),
        });
    }

    let (origin, path) =
        origin_and_path(mcp_url).ok_or_else(|| anyhow::anyhow!("invalid MCP URL '{mcp_url}'"))?;

    // Protected-resource metadata names the authorization server (and the
    // canonical resource identifier). Absent ⇒ the MCP origin doubles as
    // the authorization server (pre-9728 servers).
    let mut resource = None;
    let mut prm_scopes = None;
    let mut issuer = origin.clone();
    for url in prm_candidates(&origin, &path) {
        if let Some(meta) = fetch_json(client, &url).await {
            if let Some(first) = meta
                .get("authorization_servers")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
            {
                issuer = first.trim_end_matches('/').to_string();
            }
            resource = meta
                .get("resource")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| Some(mcp_url.to_string()));
            prm_scopes = meta
                .get("scopes_supported")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|s| !s.is_empty());
            break;
        }
    }

    let (as_origin, as_path) = origin_and_path(&issuer)
        .ok_or_else(|| anyhow::anyhow!("invalid authorization server URL '{issuer}'"))?;
    let mut meta = None;
    // Some providers (Stripe) serve the AS metadata from the RESOURCE
    // origin rather than the issuer's own origin — append those as
    // fallbacks after the RFC 8414 issuer-derived candidates.
    let mut candidates = as_metadata_candidates(&as_origin, &as_path);
    for c in as_metadata_candidates(&origin, &path) {
        if !candidates.contains(&c) {
            candidates.push(c);
        }
    }
    for url in candidates {
        if let Some(m) = fetch_json(client, &url).await {
            if m.get("authorization_endpoint").is_some() && m.get("token_endpoint").is_some() {
                meta = Some(m);
                break;
            }
        }
    }

    let get = |m: &Option<serde_json::Value>, key: &str| -> Option<String> {
        m.as_ref()
            .and_then(|m| m.get(key))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    // MCP-spec legacy fallback for servers with no metadata document at all.
    let authorize_url = cfg
        .authorize_url
        .clone()
        .or_else(|| get(&meta, "authorization_endpoint"))
        .unwrap_or_else(|| format!("{issuer}/authorize"));
    let token_url = cfg
        .token_url
        .clone()
        .or_else(|| get(&meta, "token_endpoint"))
        .unwrap_or_else(|| format!("{issuer}/token"));
    let registration_url = cfg
        .registration_url
        .clone()
        .or_else(|| get(&meta, "registration_endpoint"));

    Ok(Endpoints {
        authorize_url,
        token_url,
        registration_url,
        resource,
        scopes: cfg.scopes.clone().or(prm_scopes),
        scope_param: cfg.scope_param.clone(),
        auth_params: extra_auth_params(cfg),
    })
}

// ---------------------------------------------------------------------------
// Dynamic client registration (RFC 7591)
// ---------------------------------------------------------------------------

/// Register PeckBoard as a public client; returns `(client_id,
/// client_secret)` — the secret is usually absent for `token_endpoint_
/// auth_method: none`.
pub async fn register_client(
    client: &reqwest::Client,
    registration_url: &str,
    redirect_uri: &str,
    scopes: Option<&str>,
) -> anyhow::Result<(String, Option<String>)> {
    let mut body = serde_json::json!({
        "client_name": "PeckBoard",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    if let Some(s) = scopes {
        body["scope"] = serde_json::Value::String(s.to_string());
    }
    let resp = client.post(registration_url).json(&body).send().await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("client registration failed ({status}): {text}");
    }
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("unexpected registration response: {e}"))?;
    let client_id = v
        .get("client_id")
        .and_then(|c| c.as_str())
        .ok_or_else(|| anyhow::anyhow!("registration response has no client_id"))?
        .to_string();
    let client_secret = v
        .get("client_secret")
        .and_then(|c| c.as_str())
        .map(str::to_string);
    Ok((client_id, client_secret))
}

// ---------------------------------------------------------------------------
// Pending logins (state → everything the callback needs)
// ---------------------------------------------------------------------------

/// One in-flight login, stashed under its `state` until the provider
/// redirects back to `/oauth/callback`. In-memory on purpose: a login
/// survives neither a restart nor 15 minutes of inattention, and the
/// `state` value is the only key that can claim it.
#[derive(Debug, Clone)]
pub struct PendingLogin {
    pub server_id: String,
    pub server_name: String,
    pub verifier: String,
    pub token_url: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub token_field: Option<String>,
    pub resource: Option<String>,
    pub redirect_uri: String,
    pub created_ms: i64,
}

static PENDING: LazyLock<Mutex<HashMap<String, PendingLogin>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn stash_pending(state: String, login: PendingLogin) {
    let mut map = PENDING.lock().expect("pending logins lock");
    let cutoff = now_ms() - PENDING_TTL_MS;
    map.retain(|_, l| l.created_ms >= cutoff);
    map.insert(state, login);
}

/// Claim (and remove) the login for `state`; `None` when unknown/expired.
pub fn take_pending(state: &str) -> Option<PendingLogin> {
    let mut map = PENDING.lock().expect("pending logins lock");
    let login = map.remove(state)?;
    (login.created_ms >= now_ms() - PENDING_TTL_MS).then_some(login)
}

/// Build the authorize URL for one started login.
pub fn authorize_request_url(
    endpoints: &Endpoints,
    client_id: &str,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
) -> String {
    let mut pairs = vec![
        ("response_type", "code"),
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
    ];
    let scope_key = endpoints.scope_param.as_deref().unwrap_or("scope");
    if let Some(s) = endpoints.scopes.as_deref().filter(|s| !s.is_empty()) {
        pairs.push((scope_key, s));
    }
    if let Some(r) = endpoints.resource.as_deref() {
        pairs.push(("resource", r));
    }
    for (k, v) in &endpoints.auth_params {
        pairs.push((k.as_str(), v.as_str()));
    }
    let query = form_encode(&pairs);
    let sep = if endpoints.authorize_url.contains('?') {
        '&'
    } else {
        '?'
    };
    format!("{}{sep}{query}", endpoints.authorize_url)
}

/// Mint PKCE material and stash the pending login; returns the URL to send
/// the user's browser to.
pub fn begin_login(
    endpoints: &Endpoints,
    server: &UserMcpServer,
    client_id: String,
    client_secret: Option<String>,
    redirect_uri: String,
) -> String {
    let verifier = random_token();
    let state = random_token();
    let url = authorize_request_url(
        endpoints,
        &client_id,
        &redirect_uri,
        &challenge_for(&verifier),
        &state,
    );
    stash_pending(
        state,
        PendingLogin {
            server_id: server.id.clone(),
            server_name: server.name.clone(),
            verifier,
            token_url: endpoints.token_url.clone(),
            client_id,
            client_secret,
            token_field: server.oauth.as_ref().and_then(|c| c.token_field.clone()),
            resource: endpoints.resource.clone(),
            redirect_uri,
            created_ms: now_ms(),
        },
    );
    url
}

// ---------------------------------------------------------------------------
// Token exchange / refresh
// ---------------------------------------------------------------------------

/// A minted access token plus what is needed to renew it.
#[derive(Debug, Clone)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at_ms: Option<i64>,
}

/// Walk a dot-path (`authed_user.access_token`) through a JSON object.
fn json_path<'v>(v: &'v serde_json::Value, path: &str) -> Option<&'v serde_json::Value> {
    let mut cur = v;
    for seg in path.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

/// The dot-path sibling of `path` (same object, different leaf key).
fn sibling_path(path: &str, leaf: &str) -> String {
    match path.rsplit_once('.') {
        Some((parent, _)) => format!("{parent}.{leaf}"),
        None => leaf.to_string(),
    }
}

/// Parse a token response, honouring a non-standard `token_field` path.
/// Slack-style `{"ok":false,"error":…}` bodies fail with the error string.
fn parse_token_response(body: &str, token_field: Option<&str>) -> anyhow::Result<TokenSet> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| anyhow::anyhow!("unexpected token response: {e}"))?;
    if v.get("ok").and_then(|b| b.as_bool()) == Some(false) {
        anyhow::bail!(
            "token endpoint refused: {}",
            v.get("error").and_then(|e| e.as_str()).unwrap_or("unknown")
        );
    }
    let field = token_field.unwrap_or("access_token");
    let access_token = json_path(&v, field)
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| anyhow::anyhow!("token response has no '{field}'"))?
        .to_string();
    let pick_i64 = |leaf: &str| -> Option<i64> {
        v.get(leaf)
            .and_then(|x| x.as_i64())
            .or_else(|| json_path(&v, &sibling_path(field, leaf)).and_then(|x| x.as_i64()))
    };
    let pick_str = |leaf: &str| -> Option<String> {
        v.get(leaf)
            .and_then(|x| x.as_str())
            .or_else(|| json_path(&v, &sibling_path(field, leaf)).and_then(|x| x.as_str()))
            .map(str::to_string)
    };
    Ok(TokenSet {
        access_token,
        refresh_token: pick_str("refresh_token"),
        expires_at_ms: pick_i64("expires_in").map(|s| now_ms() + s * 1000),
    })
}

/// POST an urlencoded grant to `token_url`. The client secret (when the
/// provider issued one) goes in the form body — `client_secret_post`, the
/// method Slack and most classic OAuth2 providers accept.
async fn post_token(
    client: &reqwest::Client,
    token_url: &str,
    mut pairs: Vec<(&str, &str)>,
    client_secret: Option<&str>,
    token_field: Option<&str>,
) -> anyhow::Result<TokenSet> {
    if let Some(secret) = client_secret {
        pairs.push(("client_secret", secret));
    }
    let resp = client
        .post(token_url)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(reqwest::header::ACCEPT, "application/json")
        .body(form_encode(&pairs))
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("token request failed ({status}): {body}");
    }
    parse_token_response(&body, token_field)
}

/// Swap the callback's authorization code for tokens.
pub async fn exchange_code(
    client: &reqwest::Client,
    login: &PendingLogin,
    code: &str,
) -> anyhow::Result<TokenSet> {
    let mut pairs = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", login.redirect_uri.as_str()),
        ("client_id", login.client_id.as_str()),
        ("code_verifier", login.verifier.as_str()),
    ];
    if let Some(r) = login.resource.as_deref() {
        pairs.push(("resource", r));
    }
    post_token(
        client,
        &login.token_url,
        pairs,
        login.client_secret.as_deref(),
        login.token_field.as_deref(),
    )
    .await
}

/// Renew an access token with its refresh token.
pub async fn refresh(client: &reqwest::Client, tok: &StoredToken) -> anyhow::Result<TokenSet> {
    let refresh_token = tok
        .refresh_token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("no refresh token stored"))?;
    let mut pairs = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", tok.client_id.as_str()),
    ];
    if let Some(r) = tok.resource.as_deref() {
        pairs.push(("resource", r));
    }
    post_token(
        client,
        &tok.token_url,
        pairs,
        tok.client_secret.as_deref(),
        tok.token_field.as_deref(),
    )
    .await
}

// ---------------------------------------------------------------------------
// Token store (plugin store, one JSON map)
// ---------------------------------------------------------------------------

/// One connected server's credential + everything needed to refresh it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredToken {
    pub server_id: String,
    pub server_name: String,
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_at_ms: Option<i64>,
    pub token_url: String,
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub token_field: Option<String>,
    #[serde(default)]
    pub resource: Option<String>,
    pub obtained_at_ms: i64,
}

pub async fn load_tokens(db: &Db) -> HashMap<String, StoredToken> {
    let db = db.clone();
    tokio::task::spawn_blocking(move || {
        db.plugin_store_get_blocking(SETTINGS_NS, SETTINGS_COLLECTION, MCP_OAUTH_TOKENS_KEY)
    })
    .await
    .ok()
    .and_then(|r| r.ok())
    .flatten()
    .and_then(|raw| serde_json::from_str(&raw).ok())
    .unwrap_or_default()
}

async fn save_tokens(db: &Db, tokens: &HashMap<String, StoredToken>) -> anyhow::Result<()> {
    let value = serde_json::to_string(tokens)?;
    let db = db.clone();
    tokio::task::spawn_blocking(move || {
        db.plugin_store_put_blocking(
            SETTINGS_NS,
            SETTINGS_COLLECTION,
            MCP_OAUTH_TOKENS_KEY,
            &value,
        )
    })
    .await??;
    Ok(())
}

pub async fn put_token(db: &Db, token: StoredToken) -> anyhow::Result<()> {
    let mut tokens = load_tokens(db).await;
    tokens.insert(token.server_id.clone(), token);
    save_tokens(db, &tokens).await
}

/// Drop a server's token; `true` when one existed.
pub async fn remove_token(db: &Db, server_id: &str) -> anyhow::Result<bool> {
    let mut tokens = load_tokens(db).await;
    let existed = tokens.remove(server_id).is_some();
    if existed {
        save_tokens(db, &tokens).await?;
    }
    Ok(existed)
}

// ---------------------------------------------------------------------------
// Dispatch-time resolution
// ---------------------------------------------------------------------------

/// Serialises refreshes so a rotated refresh token is never clobbered by a
/// concurrent caller (dispatch + probe can race).
static REFRESH_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

fn is_fresh(tok: &StoredToken, now: i64) -> bool {
    match tok.expires_at_ms {
        None => true,
        Some(at) => at - now > REFRESH_MARGIN_MS,
    }
}

/// The `Authorization` header value for an OAuth server, refreshing the
/// stored token when it is near expiry. `None` when the server was never
/// connected or the token is dead beyond repair.
pub async fn bearer_for_server(db: &Db, server: &UserMcpServer) -> Option<String> {
    let tok = load_tokens(db).await.remove(&server.id)?;
    let now = now_ms();
    if is_fresh(&tok, now) {
        return Some(format!("Bearer {}", tok.access_token));
    }
    if tok.refresh_token.is_none() {
        // Expired with nothing to renew it: send it anyway (clock skew /
        // conservative expiry beats dropping the header outright).
        return Some(format!("Bearer {}", tok.access_token));
    }

    let _guard = REFRESH_LOCK.lock().await;
    // Re-read under the lock — a concurrent caller may have refreshed.
    let tok = load_tokens(db).await.remove(&server.id)?;
    if is_fresh(&tok, now_ms()) {
        return Some(format!("Bearer {}", tok.access_token));
    }
    match refresh(&http_client(), &tok).await {
        Ok(minted) => {
            let updated = StoredToken {
                access_token: minted.access_token.clone(),
                // Keep the old refresh token when the endpoint didn't rotate.
                refresh_token: minted.refresh_token.or(tok.refresh_token.clone()),
                expires_at_ms: minted.expires_at_ms,
                obtained_at_ms: now_ms(),
                ..tok
            };
            let bearer = format!("Bearer {}", updated.access_token);
            if let Err(e) = put_token(db, updated).await {
                tracing::warn!(
                    "mcp oauth: refreshed token for '{}' not persisted: {e}",
                    server.name
                );
            } else {
                tracing::info!("mcp oauth: refreshed token for '{}'", server.name);
            }
            Some(bearer)
        }
        Err(e) => {
            tracing::warn!(
                "mcp oauth: refresh failed for '{}': {e}; using the stored token as-is",
                server.name
            );
            Some(format!("Bearer {}", tok.access_token))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_and_path_splits() {
        assert_eq!(
            origin_and_path("https://mcp.linear.app/mcp"),
            Some(("https://mcp.linear.app".into(), "/mcp".into()))
        );
        assert_eq!(
            origin_and_path("https://mcp.example.com"),
            Some(("https://mcp.example.com".into(), String::new()))
        );
        assert_eq!(
            origin_and_path("https://host:8443/a/b/?x=1"),
            Some(("https://host:8443".into(), "/a/b".into()))
        );
        assert_eq!(origin_and_path("not a url"), None);
    }

    #[test]
    fn prm_and_as_candidates_are_path_aware() {
        assert_eq!(
            prm_candidates("https://h", "/mcp"),
            vec![
                "https://h/.well-known/oauth-protected-resource/mcp",
                "https://h/.well-known/oauth-protected-resource",
            ]
        );
        let cands = as_metadata_candidates("https://h", "");
        assert_eq!(
            cands,
            vec![
                "https://h/.well-known/oauth-authorization-server",
                "https://h/.well-known/openid-configuration",
            ]
        );
    }

    #[test]
    fn parse_token_response_standard_and_nested() {
        let t = parse_token_response(
            r#"{"access_token":"at","refresh_token":"rt","expires_in":3600}"#,
            None,
        )
        .unwrap();
        assert_eq!(t.access_token, "at");
        assert_eq!(t.refresh_token.as_deref(), Some("rt"));
        assert!(t.expires_at_ms.unwrap() > now_ms());

        // Slack shape: user token nested under authed_user.
        let t = parse_token_response(
            r#"{"ok":true,"access_token":"xoxb-bot","authed_user":{"access_token":"xoxp-user","expires_in":100,"refresh_token":"xoxe-1"}}"#,
            Some("authed_user.access_token"),
        )
        .unwrap();
        assert_eq!(t.access_token, "xoxp-user");
        assert_eq!(t.refresh_token.as_deref(), Some("xoxe-1"));
        assert!(t.expires_at_ms.is_some());

        let err = parse_token_response(r#"{"ok":false,"error":"invalid_code"}"#, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid_code"));

        assert!(parse_token_response(r#"{"scope":"x"}"#, None).is_err());
    }

    #[test]
    fn authorize_url_carries_pkce_scope_resource() {
        let ep = Endpoints {
            authorize_url: "https://as.example/authorize".into(),
            token_url: "https://as.example/token".into(),
            registration_url: None,
            resource: Some("https://mcp.example/mcp".into()),
            scopes: Some("read write".into()),
            scope_param: None,
            auth_params: Vec::new(),
        };
        let url = authorize_request_url(&ep, "cid", "https://pb.local/oauth/callback", "CH", "ST");
        assert!(url.starts_with("https://as.example/authorize?response_type=code"));
        assert!(url.contains("client_id=cid"));
        assert!(url.contains("code_challenge=CH"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=ST"));
        assert!(url.contains("scope=read%20write"));
        assert!(url.contains("resource=https%3A%2F%2Fmcp.example%2Fmcp"));
        // Existing query string extends with '&'.
        let ep2 = Endpoints {
            authorize_url: "https://as.example/authorize?tenant=a".into(),
            ..ep
        };
        assert!(
            authorize_request_url(&ep2, "cid", "r", "c", "s")
                .starts_with("https://as.example/authorize?tenant=a&response_type=code")
        );
        // Slack-style scope param rename.
        let ep3 = Endpoints {
            scope_param: Some("user_scope".into()),
            authorize_url: "https://slack.com/oauth/v2_user/authorize".into(),
            ..ep2
        };
        let u = authorize_request_url(&ep3, "cid", "r", "c", "s");
        assert!(u.contains("user_scope=read%20write"));
        assert!(!u.contains("&scope="));
        // Extra SSO params ride along on the authorize URL.
        let ep4 = Endpoints {
            auth_params: vec![("team".into(), "T0123ABC".into())],
            ..ep3
        };
        let u = authorize_request_url(&ep4, "cid", "r", "c", "s");
        assert!(u.contains("team=T0123ABC"));
    }

    #[test]
    fn extra_auth_params_filters_reserved_and_blank() {
        use super::super::user_servers::KvEntry;
        let cfg = McpOauthConfig {
            auth_params: vec![
                KvEntry {
                    key: "team".into(),
                    value: "T1".into(),
                },
                KvEntry {
                    key: "state".into(),
                    value: "evil".into(),
                },
                KvEntry {
                    key: "  ".into(),
                    value: "y".into(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            extra_auth_params(&cfg),
            vec![("team".to_string(), "T1".to_string())]
        );
    }

    #[test]
    fn pending_login_roundtrip_and_ttl() {
        let mk = |created_ms| PendingLogin {
            server_id: "sid".into(),
            server_name: "linear".into(),
            verifier: "v".into(),
            token_url: "https://as/token".into(),
            client_id: "c".into(),
            client_secret: None,
            token_field: None,
            resource: None,
            redirect_uri: "https://pb/oauth/callback".into(),
            created_ms,
        };
        stash_pending("st-live".into(), mk(now_ms()));
        assert!(take_pending("st-live").is_some());
        assert!(take_pending("st-live").is_none(), "single-use");
        stash_pending("st-old".into(), mk(now_ms() - PENDING_TTL_MS - 1000));
        assert!(take_pending("st-old").is_none(), "expired");
        assert!(take_pending("st-unknown").is_none());
    }

    #[test]
    fn sibling_path_walks() {
        assert_eq!(sibling_path("access_token", "expires_in"), "expires_in");
        assert_eq!(
            sibling_path("authed_user.access_token", "refresh_token"),
            "authed_user.refresh_token"
        );
    }
}
