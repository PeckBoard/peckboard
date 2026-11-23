//! HTTP surface: the catalog, publish and yank (the rest — handles, skill
//! info, forks, merge requests — is in [`crate::collab`]).
//!
//! Read side (public, cacheable): `GET /index.json`,
//! `GET /skills/<name>/<version>/SKILL.md`, `GET /groups/<slug>.json`.
//!
//! Write side: `GET /api/challenge?public_key=…` hands out a single-use
//! nonce (and the PoW a new key must solve); `POST /api/publish` and
//! `POST /api/yank` carry an Ed25519 signature over it in the JSON body,
//! every other write carries it in `X-Peck-*` headers ([`verify_signed`]).
//! Errors are `{error, code, findings?}` with a stable `code`.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use peckboard_skill_format::format;
use peckboard_skill_format::guard::{self, Finding, NoSecrets};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::store::commit_version;
use crate::{Hub, MAX_SKILL_BYTES, auth};

/// Raw request cap: `skill_md` (≤ 512 KiB) can double under JSON escaping.
const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024 + 16 * 1024;
const MAX_YANK_BYTES: usize = 16 * 1024;

#[derive(Debug, Serialize)]
pub struct ApiError {
    #[serde(skip)]
    pub status: StatusCode,
    pub error: String,
    pub code: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub findings: Option<Vec<Finding>>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, error: impl Into<String>) -> Self {
        ApiError {
            status,
            error: error.into(),
            code,
            findings: None,
        }
    }
    pub(crate) fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", msg)
    }
    pub(crate) fn internal(e: anyhow::Error) -> Self {
        warn!("internal error: {e:#}");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "Something went wrong on the skills server. Try again later.",
        )
    }
    pub(crate) fn not_found(what: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("{what} not found."),
        )
    }
    pub(crate) fn not_owner(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "not_owner", msg)
    }
    pub(crate) fn conflict(code: &'static str, msg: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, msg)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(&self)).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::internal(e)
    }
}

pub(crate) type ApiResult<T> = Result<T, ApiError>;

pub(crate) fn content_findings(findings: Vec<Finding>) -> ApiError {
    let mut e = ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "content_findings",
        "The skill looks like it contains a credential or an internal detail. Remove it and try again.",
    );
    e.findings = Some(findings);
    e
}

pub(crate) fn rate_limited(per_day: u32) -> ApiError {
    ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        format!(
            "This key has published {per_day} skills today, the daily limit. Try again tomorrow (UTC)."
        ),
    )
}

pub fn router(hub: Arc<Hub>) -> Router {
    let r = Router::new()
        .route("/", get(root))
        .route("/index.json", get(index))
        .route("/skills/{name}/{version}/SKILL.md", get(skill_md))
        .route("/groups/{file}", get(group))
        .route("/api/challenge", get(challenge))
        .route("/api/publish", post(publish))
        .route("/api/yank", post(yank));
    crate::collab::routes(r)
        // Bodies are read by hand so an oversized one gets a JSON error.
        .layer(DefaultBodyLimit::disable())
        .with_state(hub)
}

async fn root() -> &'static str {
    "PeckBoard skills hub. Catalog: /index.json\n"
}

async fn index(State(hub): State<Arc<Hub>>) -> ApiResult<Response> {
    let idx = hub.index()?;
    Ok(([(header::CACHE_CONTROL, "public, max-age=60")], Json(idx)).into_response())
}

async fn skill_md(
    State(hub): State<Arc<Hub>>,
    Path((name, version)): Path<(String, String)>,
) -> ApiResult<Response> {
    if !format::valid_skill_name(&name) {
        return Err(ApiError::not_found("Skill"));
    }
    // `main` = the current head (after a sync or an unpublished merge), not
    // a release: never cached, gone with the skill.
    if version == "main" {
        crate::collab::visible(&hub.store(), &name)?;
        let text = hub.repos.open_repo(&name).ok().and_then(|r| {
            crate::git::main_head(&r).and_then(|h| crate::git::skill_at(&r, h).ok().flatten())
        });
        return match text {
            Some(text) => Ok((
                [
                    (header::CONTENT_TYPE, "text/markdown; charset=utf-8"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                text,
            )
                .into_response()),
            None => Err(ApiError::not_found("Skill")),
        };
    }
    match hub.store().served_skill(&hub.repos, &name, &version)? {
        Some(text) => Ok((
            [
                (header::CONTENT_TYPE, "text/markdown; charset=utf-8"),
                (header::CACHE_CONTROL, "public, max-age=300"),
            ],
            text,
        )
            .into_response()),
        None => Err(ApiError::not_found("Skill version")),
    }
}

async fn group(State(hub): State<Arc<Hub>>, Path(file): Path<String>) -> ApiResult<Response> {
    let Some(slug) = file.strip_suffix(".json").filter(|s| format::valid_name(s)) else {
        return Err(ApiError::not_found("Group"));
    };
    if hub.store().group(slug)?.is_none() {
        return Err(ApiError::not_found("Group"));
    }
    match hub.index()?.groups.into_iter().find(|g| g.slug == slug) {
        Some(g) => Ok(([(header::CACHE_CONTROL, "public, max-age=60")], Json(g)).into_response()),
        None => Err(ApiError::not_found("Group")),
    }
}

#[derive(Deserialize)]
struct ChallengeQuery {
    public_key: String,
}

#[derive(Serialize)]
struct ChallengeResponse {
    nonce: String,
    expires_in: u64,
    pow_difficulty: u32,
}

pub(crate) fn bad_key() -> ApiError {
    ApiError::bad_request("public_key must be a base64 Ed25519 public key (32 bytes).")
}

async fn challenge(
    State(hub): State<Arc<Hub>>,
    Query(q): Query<ChallengeQuery>,
) -> ApiResult<Json<ChallengeResponse>> {
    let (_, key) = auth::parse_public_key(&q.public_key).ok_or_else(bad_key)?;
    let publisher = hub.store().publisher(&key)?;
    if publisher.as_ref().is_some_and(|p| p.banned) {
        return Err(banned());
    }
    let pow_difficulty = if publisher.is_some() {
        0
    } else {
        hub.cfg.new_key_pow_bits
    };
    let nonce = hub.nonces().issue(&key, pow_difficulty).ok_or_else(|| {
        ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "The skills server is busy. Try again in a few minutes.",
        )
    })?;
    Ok(Json(ChallengeResponse {
        nonce,
        expires_in: auth::NONCE_TTL.as_secs(),
        pow_difficulty,
    }))
}

fn banned() -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "banned",
        "This publisher key has been banned from the public skills hub.",
    )
}

fn nonce_invalid() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "nonce_invalid",
        "The challenge expired or was already used. Ask for a new one and sign again.",
    )
}

fn bad_signature() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "bad_signature",
        "The signature does not match this key and request.",
    )
}

fn pow_required(bits: u32) -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "pow_required",
        format!("A first request from a new key needs proof of work ({bits} leading zero bits)."),
    )
}

pub(crate) async fn read_body(body: Body, cap: usize) -> ApiResult<axum::body::Bytes> {
    axum::body::to_bytes(body, cap).await.map_err(|_| {
        ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "The request is too large.",
        )
    })
}

/// Authenticate a signed-header request (`X-Peck-Key`, `X-Peck-Nonce`,
/// `X-Peck-Pow`, `X-Peck-Signature`) over `METHOD`, the path and the raw
/// body. Consumes the nonce, checks PoW and bans, and records the key as
/// seen. Returns the key's canonical text form.
pub(crate) fn verify_signed(
    hub: &Hub,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
) -> ApiResult<String> {
    let h = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let missing = |what: &str| {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "bad_signature",
            format!("This request must be signed: the {what} header is missing."),
        )
    };
    let (vk, key) = auth::parse_public_key(h(auth::H_KEY).ok_or_else(|| missing("X-Peck-Key"))?)
        .ok_or_else(bad_key)?;
    let nonce = h(auth::H_NONCE).ok_or_else(|| missing("X-Peck-Nonce"))?;
    let signature = h(auth::H_SIGNATURE).ok_or_else(|| missing("X-Peck-Signature"))?;
    let pow_bits = hub.nonces().take(nonce, &key).ok_or_else(nonce_invalid)?;
    let msg = auth::request_message(method.as_str(), uri.path(), nonce, body);
    if !auth::verify(&vk, &msg, signature) {
        return Err(bad_signature());
    }
    if !auth::pow_ok(nonce, h(auth::H_POW).unwrap_or(""), pow_bits) {
        return Err(pow_required(pow_bits));
    }
    let store = hub.store();
    if store.publisher(&key)?.is_some_and(|p| p.banned) {
        return Err(banned());
    }
    store.ensure_publisher(&key)?;
    Ok(key)
}

#[derive(Deserialize)]
struct PublishRequest {
    skill_md: String,
    public_key: String,
    nonce: String,
    #[serde(default)]
    pow: Option<String>,
    signature: String,
}

#[derive(Serialize)]
struct PublishResponse {
    name: String,
    version: String,
    url: String,
}

async fn publish(State(hub): State<Arc<Hub>>, body: Body) -> ApiResult<Response> {
    let raw = read_body(body, MAX_REQUEST_BYTES).await?;
    let req: PublishRequest = serde_json::from_slice(&raw).map_err(|e| {
        ApiError::bad_request(format!(
            "Expected JSON {{skill_md, public_key, nonce, pow?, signature}}: {e}"
        ))
    })?;
    if req.skill_md.len() > MAX_SKILL_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            format!(
                "SKILL.md is {} KiB; the limit is {} KiB.",
                req.skill_md.len() / 1024,
                MAX_SKILL_BYTES / 1024
            ),
        ));
    }
    let (vk, key) = auth::parse_public_key(&req.public_key).ok_or_else(bad_key)?;
    let pow_bits = hub
        .nonces()
        .take(&req.nonce, &key)
        .ok_or_else(nonce_invalid)?;
    if !auth::verify(
        &vk,
        &auth::publish_message(&req.nonce, &req.skill_md),
        &req.signature,
    ) {
        return Err(bad_signature());
    }
    if !auth::pow_ok(&req.nonce, req.pow.as_deref().unwrap_or(""), pow_bits) {
        return Err(pow_required(pow_bits));
    }

    let doc = format::parse_skill(&req.skill_md)
        .map_err(|e| ApiError::bad_request(format!("SKILL.md: {e}.")))?;
    if let Some(g) = doc.groups.iter().find(|g| !format::valid_name(g)) {
        return Err(ApiError::bad_request(format!(
            "SKILL.md: invalid group slug '{g}' (lowercase letters, digits and dashes)."
        )));
    }
    let fork = format::split_fork_name(&doc.name).is_some();

    let today = crate::today_utc();
    // Repo lock first (it serializes this name's check-then-write), then
    // the store.
    let lock = hub.repos.lock(&doc.name);
    let mut store = hub.store();
    let publisher = store.publisher(&key)?;
    if publisher.as_ref().is_some_and(|p| p.banned) {
        return Err(banned());
    }
    // Signed + PoW paid: the key counts as seen from now on.
    store.ensure_publisher(&key)?;
    if store.publishes_today(&key, &today)? >= hub.cfg.publishes_per_day {
        return Err(rate_limited(hub.cfg.publishes_per_day));
    }
    let row = store.skill(&doc.name)?;
    match &row {
        None if fork => {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "not_found",
                format!(
                    "There is no fork named '{}'. Fork the skill first.",
                    doc.name
                ),
            ));
        }
        Some(r) if r.owner_key != key && fork => {
            return Err(ApiError::not_owner(format!(
                "Only the owner of the fork '{}' can publish to it.",
                doc.name
            )));
        }
        Some(r) if r.owner_key != key => {
            return Err(ApiError::conflict(
                "name_taken",
                format!(
                    "The name '{}' belongs to another publisher. Rename the skill and try again.",
                    doc.name
                ),
            ));
        }
        _ => {}
    }
    if store.version(&doc.name, &doc.version)?.is_some() {
        return Err(ApiError::conflict(
            "version_exists",
            format!(
                "Version {} of '{}' is already published. Bump the version and try again.",
                doc.version, doc.name
            ),
        ));
    }
    let findings = guard::check_skill(&doc, &NoSecrets);
    if !findings.is_empty() {
        return Err(content_findings(findings));
    }
    commit_version(
        &mut store,
        &hub.repos,
        &lock,
        &key,
        &doc,
        &req.skill_md,
        None,
        &format!("Publish {}", doc.version),
        Some(&today),
    )?;
    store.audit(
        "publish",
        Some(&key),
        &format!("{}@{}", doc.name, doc.version),
    );
    drop(store);
    drop(lock);
    hub.invalidate_index();
    info!(name = %doc.name, version = %doc.version, "published");
    if let Some(parent) = row.and_then(|r| r.parent) {
        crate::collab::refresh_fork(&hub, &doc.name, &parent);
    }
    Ok((
        StatusCode::CREATED,
        Json(PublishResponse {
            url: hub.skill_url(&doc.name, &doc.version),
            name: doc.name,
            version: doc.version,
        }),
    )
        .into_response())
}

#[derive(Deserialize)]
struct YankRequest {
    name: String,
    version: String,
    public_key: String,
    nonce: String,
    signature: String,
}

async fn yank(State(hub): State<Arc<Hub>>, body: Body) -> ApiResult<Response> {
    let raw = read_body(body, MAX_YANK_BYTES).await?;
    let req: YankRequest = serde_json::from_slice(&raw).map_err(|e| {
        ApiError::bad_request(format!(
            "Expected JSON {{name, version, public_key, nonce, signature}}: {e}"
        ))
    })?;
    let (vk, key) = auth::parse_public_key(&req.public_key).ok_or_else(bad_key)?;
    hub.nonces()
        .take(&req.nonce, &key)
        .ok_or_else(nonce_invalid)?;
    if !auth::verify(
        &vk,
        &auth::yank_message(&req.nonce, &req.name, &req.version),
        &req.signature,
    ) {
        return Err(bad_signature());
    }
    let store = hub.store();
    match store.version(&req.name, &req.version)? {
        Some(v) if !v.removed => {}
        _ => return Err(ApiError::not_found("Skill version")),
    }
    if store.skill_owner(&req.name)?.as_deref() != Some(key.as_str()) {
        return Err(ApiError::not_owner(
            "Only the skill's publisher can yank it.",
        ));
    }
    store.set_yanked(&req.name, &req.version, true)?;
    store.audit("yank", Some(&key), &format!("{}@{}", req.name, req.version));
    drop(store);
    hub.invalidate_index();
    Ok(Json(serde_json::json!({
        "name": req.name,
        "version": req.version,
        "yanked": true,
    }))
    .into_response())
}
