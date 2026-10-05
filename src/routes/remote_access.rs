//! `/api/remote-access/*` — remote access through the relay. Admin-only:
//! the setting opens a path into this host from the internet, and every
//! paired device gets the same reach as the admin's browser.
//!
//! The link secret is minted by `POST /api/remote-access/devices` (and
//! `POST …/devices/{id}/link`, which re-issues an unused one) and handed
//! back EXACTLY ONCE, inside the pairing link (+ its QR code). It is stored
//! only sealed (`service::remote_access::secret`) and no response shape
//! after creation carries it — `DeviceView` is built field by field and
//! `RemoteDevice` isn't even `Serialize`.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, patch, post},
};
use qrcode::QrCode;
use serde::{Deserialize, Serialize};

use crate::auth::middleware::{AuthUser, require_admin, require_auth};
use crate::db::models::{NewRemoteDevice, RemoteDevice, RemoteDeviceEnrollment, enrollment_state};
use crate::service::remote_access::{
    DeviceStatus, RemoteAccess, RemoteAccessSettings, dev_link_ttl_enabled, enrollment_view_state,
    link_ttl, secret, validate_direct, validate_relay_host,
};
use crate::state::AppState;

const NAME_MAX_LEN: usize = 128;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/remote-access", get(overview).put(put_settings))
        .route(
            "/api/remote-access/registration/refresh",
            post(refresh_registration),
        )
        .route("/api/remote-access/devices", post(create))
        .route(
            "/api/remote-access/devices/{id}",
            patch(rename).delete(revoke),
        )
        .route("/api/remote-access/devices/{id}/link", post(reissue_link))
        .route_layer(middleware::from_fn(require_admin))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}

fn internal_err(e: impl std::fmt::Display) -> Response {
    err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
}

fn valid_name(raw: &str) -> Option<String> {
    let name = raw.trim().to_string();
    (!name.is_empty() && name.chars().count() <= NAME_MAX_LEN).then_some(name)
}

/// Public metadata + live tunnel status + pairing-v2 state. Never a
/// secret.
#[derive(Serialize)]
struct DeviceView {
    id: String,
    name: String,
    created_at: String,
    last_connected_at: Option<String>,
    status: DeviceStatus,
    /// `legacy` | `pending` | `expired` | `staged` | `enrolled`.
    enrollment: &'static str,
    link_expires_at: Option<String>,
    enrolled_at: Option<String>,
    enrolled_from: Option<String>,
    device_name_hint: Option<String>,
    activated_at: Option<String>,
    reuse_attempts: i32,
    last_reuse_at: Option<String>,
    last_reuse_from: Option<String>,
}

impl DeviceView {
    fn of(d: &RemoteDevice, enr: Option<&RemoteDeviceEnrollment>, ra: &RemoteAccess) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        DeviceView {
            id: d.id.clone(),
            name: d.name.clone(),
            created_at: d.created_at.clone(),
            last_connected_at: d.last_connected_at.clone(),
            status: ra.status(&d.id),
            enrollment: enrollment_view_state(enr, &now),
            link_expires_at: enr.and_then(|e| e.link_expires_at.clone()),
            enrolled_at: enr.and_then(|e| e.enrolled_at.clone()),
            enrolled_from: enr.and_then(|e| e.enrolled_from.clone()),
            device_name_hint: enr.and_then(|e| e.device_name_hint.clone()),
            activated_at: enr.and_then(|e| e.activated_at.clone()),
            reuse_attempts: enr.map_or(0, |e| e.reuse_attempts),
            last_reuse_at: enr.and_then(|e| e.last_reuse_at.clone()),
            last_reuse_from: enr.and_then(|e| e.last_reuse_from.clone()),
        }
    }

    /// The view of one device, with its enrollment row looked up.
    async fn load(state: &AppState, d: &RemoteDevice) -> Result<Self, Response> {
        let enr = state
            .db
            .get_remote_device_enrollment(&d.id)
            .await
            .map_err(internal_err)?;
        Ok(Self::of(d, enr.as_ref(), &state.remote_access))
    }
}

fn settings_json(s: &RemoteAccessSettings) -> serde_json::Map<String, serde_json::Value> {
    let serde_json::Value::Object(m) = serde_json::json!({
        "enabled": s.enabled,
        "relay_host": s.relay_host,
        "udp_port_base": s.udp_port_base,
        "udp_port_count": s.udp_port_count,
        "public_address": s.public_address,
    }) else {
        unreachable!()
    };
    m
}

/// GET /api/remote-access — the settings (`enabled`, `relay_host`,
/// `udp_port_base`, `udp_port_count`, `public_address`), `devices`,
/// `registration` (`{supported, registered, gated, url}`: the box
/// identity's relay registration; `supported: false` + nulls until the
/// relay has said anything, e.g. one predating relay registration) and
/// `box_fingerprint` (null until the identity exists).
async fn overview(State(state): State<Arc<AppState>>) -> Response {
    let ra = &state.remote_access;
    let settings = ra.settings().await;
    let devices = match state.db.list_remote_devices().await {
        Ok(d) => d,
        Err(e) => return internal_err(e),
    };
    let enrollments = match state.db.list_remote_device_enrollments().await {
        Ok(e) => e,
        Err(e) => return internal_err(e),
    };
    let views: Vec<DeviceView> = devices
        .iter()
        .map(|d| {
            let enr = enrollments.iter().find(|e| e.device_id == d.id);
            DeviceView::of(d, enr, ra)
        })
        .collect();
    let mut body = settings_json(&settings);
    body.insert("devices".into(), serde_json::json!(views));
    body.insert(
        "registration".into(),
        serde_json::json!(ra.registration(&settings.relay_host)),
    );
    body.insert(
        "box_fingerprint".into(),
        serde_json::json!(ra.box_fingerprint()),
    );
    Json(body).into_response()
}

/// POST /api/remote-access/registration/refresh — ask the relay now whether
/// this box is registered (creating the box identity if needed; at most
/// one request per 5 s) → the overview's `registration` object. The UI
/// polls this while it waits for the admin to finish registering.
async fn refresh_registration(State(state): State<Arc<AppState>>) -> Response {
    Json(state.remote_access.refresh_registration().await).into_response()
}

#[derive(Deserialize)]
struct SettingsBody {
    enabled: Option<bool>,
    relay_host: Option<String>,
    /// Direct-connection fields, always sent together (a `null` port base
    /// means ephemeral ports).
    direct: Option<DirectBody>,
}

#[derive(Deserialize)]
struct DirectBody {
    udp_port_base: Option<i64>,
    udp_port_count: i64,
    #[serde(default)]
    public_address: String,
}

/// PUT /api/remote-access — `{enabled?, relay_host?, direct?}` → the new
/// settings. Restarts every device loop under the new values. A rejected
/// direct field answers 400 `{error, field}`.
async fn put_settings(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SettingsBody>,
) -> Response {
    let mut s: RemoteAccessSettings = state.remote_access.settings().await;
    if let Some(enabled) = body.enabled {
        s.enabled = enabled;
    }
    if let Some(host) = body.relay_host {
        let host = host.trim().to_string();
        if let Err(msg) = validate_relay_host(&host) {
            return err(StatusCode::BAD_REQUEST, msg);
        }
        s.relay_host = host;
    }
    if let Some(d) = body.direct {
        match validate_direct(d.udp_port_base, d.udp_port_count, &d.public_address) {
            Ok(v) => {
                s.udp_port_base = v.udp_port_base;
                s.udp_port_count = v.udp_port_count;
                s.public_address = v.public_address;
            }
            Err((field, msg)) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": msg, "field": field })),
                )
                    .into_response();
            }
        }
    }
    if let Err(e) = state.remote_access.put_settings(&s).await {
        return internal_err(e);
    }
    tracing::info!(enabled = s.enabled, "remote access setting changed");
    Json(settings_json(&s)).into_response()
}

#[derive(Deserialize)]
struct NameBody {
    name: String,
}

#[derive(Deserialize)]
struct CreateBody {
    name: String,
    /// Per-link TTL override; honoured only under the dev TTL knob
    /// (`PECKBOARD_DEV_LINK_TTL_SECS`), ignored otherwise.
    #[serde(default)]
    ttl_secs: Option<u64>,
}

#[derive(Deserialize, Default)]
struct ReissueBody {
    #[serde(default)]
    ttl_secs: Option<u64>,
}

/// The TTL a new link gets (see [`CreateBody::ttl_secs`]).
fn effective_ttl(requested: Option<u64>) -> Duration {
    match requested.filter(|s| *s > 0) {
        Some(secs) if dev_link_ttl_enabled() => Duration::from_secs(secs),
        _ => link_ttl(),
    }
}

/// A fresh sealed link secret for `id` and the link it encodes, expiring
/// `ttl` from now.
struct MintedLink {
    secret_ciphertext: Vec<u8>,
    secret_nonce: Vec<u8>,
    link: peckboard_relay::tunnel::PairingLink,
    expires_at: String,
    box_fingerprint: String,
}

async fn mint_link(state: &AppState, id: &str, ttl: Duration) -> Result<MintedLink, Response> {
    let identity = state
        .remote_access
        .link_identity()
        .await
        .map_err(|e| err(StatusCode::CONFLICT, &format!("{e:#}")))?;
    let s = secret::DeviceSecret::generate();
    let (secret_ciphertext, secret_nonce) =
        secret::seal(state.remote_access.vault_key(), id, &s).map_err(internal_err)?;
    let expires = chrono::Utc::now() + chrono::Duration::from_std(ttl).map_err(internal_err)?;
    let relay_host = state.remote_access.settings().await.relay_host;
    let link = s.link_v2(
        &relay_host,
        identity.public_key(),
        expires.timestamp().max(0) as u64,
    );
    Ok(MintedLink {
        secret_ciphertext,
        secret_nonce,
        link,
        expires_at: expires.to_rfc3339(),
        box_fingerprint: identity.fingerprint(),
    })
}

/// `{device, pairing_link, app_link, qr_svg, expires_at, box_fingerprint}`
/// — the only response shape that carries a link secret.
fn link_response(status: StatusCode, device: DeviceView, minted: &MintedLink) -> Response {
    let https = minted.link.to_https();
    let qr_svg = QrCode::new(https.as_bytes())
        .map(|c| {
            c.render::<qrcode::render::svg::Color>()
                .min_dimensions(200, 200)
                .build()
        })
        .unwrap_or_default();
    (
        status,
        Json(serde_json::json!({
            "device": device,
            "pairing_link": https,
            "app_link": minted.link.to_uri(),
            "qr_svg": qr_svg,
            "expires_at": minted.expires_at,
            "box_fingerprint": minted.box_fingerprint,
        })),
    )
        .into_response()
}

/// POST /api/remote-access/devices `{name}` → 201
/// `{device, pairing_link, app_link, qr_svg, expires_at, box_fingerprint}`.
/// `pairing_link` is the `https://peckboard.com/pair#…` form (what the QR
/// encodes), `app_link` the `peckboard://` form. The link pins this box's
/// identity key, works once, and expires after an hour. 409 when the box
/// identity file is missing while devices still pin it.
async fn create(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Json(body): Json<CreateBody>,
) -> Response {
    let Some(name) = valid_name(&body.name) else {
        return err(StatusCode::BAD_REQUEST, "name must be 1..=128 chars");
    };
    let id = uuid::Uuid::new_v4().to_string();
    let minted = match mint_link(&state, &id, effective_ttl(body.ttl_secs)).await {
        Ok(m) => m,
        Err(resp) => return resp,
    };
    let row = match state
        .db
        .insert_remote_device_with_link(
            NewRemoteDevice {
                id,
                user_id: user.user_id.clone(),
                name,
                secret_ciphertext: minted.secret_ciphertext.clone(),
                secret_nonce: minted.secret_nonce.clone(),
                created_at: chrono::Utc::now().to_rfc3339(),
                last_connected_at: None,
            },
            minted.expires_at.clone(),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return internal_err(e),
    };
    state.remote_access.reconcile().await;
    let view = match DeviceView::load(&state, &row).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    link_response(StatusCode::CREATED, view, &minted)
}

/// POST /api/remote-access/devices/:id/link → 200, the same shape as
/// create: a fresh link (new secret, new hour) for a device whose link
/// was never used — unused or expired. 409 for a device that already
/// enrolled (staged / enrolled) or a legacy pairing: revoke and pair again
/// instead.
async fn reissue_link(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<ReissueBody>>,
) -> Response {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let row = match state.db.get_remote_device(&id).await {
        Ok(Some(d)) => d,
        Ok(None) => return err(StatusCode::NOT_FOUND, "no such device"),
        Err(e) => return internal_err(e),
    };
    match state.db.get_remote_device_enrollment(&id).await {
        Ok(Some(e)) if e.state == enrollment_state::PENDING => {}
        Ok(Some(_)) => {
            return err(
                StatusCode::CONFLICT,
                "this device already enrolled with its link; revoke it and pair again",
            );
        }
        Ok(None) => {
            return err(
                StatusCode::CONFLICT,
                "this device was paired with an older link that can't be re-issued; revoke it and pair again",
            );
        }
        Err(e) => return internal_err(e),
    }
    let minted = match mint_link(&state, &id, effective_ttl(body.ttl_secs)).await {
        Ok(m) => m,
        Err(resp) => return resp,
    };
    match state
        .db
        .reissue_remote_device_link(
            &id,
            minted.secret_ciphertext.clone(),
            minted.secret_nonce.clone(),
            &minted.expires_at,
        )
        .await
    {
        Ok(true) => {}
        Ok(false) => return err(StatusCode::CONFLICT, "this link was used meanwhile"),
        Err(e) => return internal_err(e),
    }
    // The running rid(S) loop holds the old secret.
    state.remote_access.restart_device(&id).await;
    tracing::info!(device_id = %id, "remote access: pairing link re-issued");
    let view = match DeviceView::load(&state, &row).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    link_response(StatusCode::OK, view, &minted)
}

/// PATCH /api/remote-access/devices/:id `{name}` → `{device}`.
async fn rename(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<NameBody>,
) -> Response {
    let Some(name) = valid_name(&body.name) else {
        return err(StatusCode::BAD_REQUEST, "name must be 1..=128 chars");
    };
    match state.db.rename_remote_device(&id, &name).await {
        Ok(false) => return err(StatusCode::NOT_FOUND, "no such device"),
        Ok(true) => {}
        Err(e) => return internal_err(e),
    }
    match state.db.get_remote_device(&id).await {
        Ok(Some(d)) => match DeviceView::load(&state, &d).await {
            Ok(v) => Json(serde_json::json!({ "device": v })).into_response(),
            Err(resp) => resp,
        },
        Ok(None) => err(StatusCode::NOT_FOUND, "no such device"),
        Err(e) => internal_err(e),
    }
}

/// DELETE /api/remote-access/devices/:id → 204. Deletes the row and its
/// enrollment (and so every sealed secret — the device's rendezvous ids
/// die with them), drops the live tunnels and every connection through
/// them immediately, and revokes the auth sessions created through the
/// device (their WebSockets close on their next session check).
async fn revoke(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match state.db.delete_remote_device(&id).await {
        Ok(true) => {
            state.remote_access.stop_device(&id);
            let sessions = match state.db.delete_auth_sessions_by_remote_device(&id).await {
                Ok(n) => n,
                Err(e) => return internal_err(e),
            };
            tracing::info!(device_id = %id, sessions, "remote access device revoked");
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => err(StatusCode::NOT_FOUND, "no such device"),
        Err(e) => internal_err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::middleware::tests::{seed_authenticated_user, test_state, test_state_with};
    use crate::db::Db;
    use crate::db::crud::{EnrollAttempt, EnrollOutcome};
    use crate::service::remote_access::{IDENTITY_FILE, testing::FakeBackend};
    use axum::body::Body;
    use axum::http::{Request, header};
    use std::sync::atomic::Ordering::SeqCst;
    use tower::ServiceExt;

    async fn call(
        state: &Arc<AppState>,
        token: &str,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let b = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"));
        let req = match body {
            Some(v) => b
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let resp = router(state.clone())
            .with_state(state.clone())
            .oneshot(req)
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    /// `s=` of an https pairing link.
    fn link_secret_b64(link: &str) -> String {
        let frag = link.split('#').nth(1).unwrap();
        frag.split('&')
            .find_map(|kv| kv.strip_prefix("s="))
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn non_admin_is_forbidden_everywhere() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "user").await;
        for (method, uri, body) in [
            ("GET", "/api/remote-access", None),
            (
                "PUT",
                "/api/remote-access",
                Some(serde_json::json!({ "enabled": true })),
            ),
            ("POST", "/api/remote-access/registration/refresh", None),
            (
                "POST",
                "/api/remote-access/devices",
                Some(serde_json::json!({ "name": "phone" })),
            ),
            (
                "PATCH",
                "/api/remote-access/devices/x",
                Some(serde_json::json!({ "name": "y" })),
            ),
            ("POST", "/api/remote-access/devices/x/link", None),
            ("DELETE", "/api/remote-access/devices/x", None),
        ] {
            let (status, _) = call(&state, &token, method, uri, body).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
        }
        assert!(state.db.list_remote_devices().await.unwrap().is_empty());
    }

    /// Create hands out the https v2 link (pinning this box, expiring in an
    /// hour) with the box fingerprint, exactly once; the list and rename
    /// never carry the secret; a legacy row shows as such; revoke deletes
    /// the row.
    #[tokio::test]
    async fn pair_once_list_without_secret_rename_revoke() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "admin").await;

        let (status, body) = call(&state, &token, "GET", "/api/remote-access", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["enabled"], false, "off by default");
        assert_eq!(body["relay_host"], "relay.peckboard.com");
        assert_eq!(body["box_fingerprint"], serde_json::Value::Null);

        let (status, body) = call(
            &state,
            &token,
            "POST",
            "/api/remote-access/devices",
            Some(serde_json::json!({ "name": " phone " })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let link = body["pairing_link"].as_str().unwrap().to_string();
        assert!(
            link.starts_with("https://peckboard.com/pair#v=2&s="),
            "{link}"
        );
        assert!(!link.contains("&r="), "default relay is implied: {link}");
        let parsed = peckboard_relay::tunnel::PairingLink::parse(&link).unwrap();
        let app_link = body["app_link"].as_str().unwrap();
        assert!(app_link.starts_with("peckboard://pair/"), "{app_link}");
        assert_eq!(
            peckboard_relay::tunnel::PairingLink::parse(app_link)
                .unwrap()
                .secret
                .as_bytes(),
            parsed.secret.as_bytes()
        );
        let fp = body["box_fingerprint"].as_str().unwrap().to_string();
        assert_eq!(parsed.box_fingerprint().as_deref(), Some(fp.as_str()));
        assert_eq!(fp.len(), 19, "XXXX-XXXX-XXXX-XXXX: {fp}");
        let expires = body["expires_at"].as_str().unwrap();
        let exp = chrono::DateTime::parse_from_rfc3339(expires).unwrap();
        let ttl = exp.with_timezone(&chrono::Utc) - chrono::Utc::now();
        assert!(ttl.num_minutes() >= 59 && ttl.num_minutes() <= 60, "{ttl}");
        assert_eq!(parsed.expires, Some(exp.timestamp() as u64));
        assert!(body["qr_svg"].as_str().unwrap().contains("<svg"));
        assert_eq!(body["device"]["name"], "phone");
        assert_eq!(body["device"]["enrollment"], "pending");
        assert_eq!(body["device"]["link_expires_at"], expires);
        assert!(dir.path().join(IDENTITY_FILE).exists());
        let id = body["device"]["id"].as_str().unwrap().to_string();
        let encoded = link_secret_b64(&link);

        // The secret is stored sealed, and opens back to the linked value.
        let row = state.db.get_remote_device(&id).await.unwrap().unwrap();
        let opened = secret::open(state.remote_access.vault_key(), &row).unwrap();
        use base64::Engine as _;
        assert_eq!(
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(opened.as_bytes()),
            encoded
        );
        assert!(!row.secret_ciphertext.is_empty());

        // Never returned again: not by the list, not by rename.
        let (_, list) = call(&state, &token, "GET", "/api/remote-access", None).await;
        let (_, renamed) = call(
            &state,
            &token,
            "PATCH",
            &format!("/api/remote-access/devices/{id}"),
            Some(serde_json::json!({ "name": "tablet" })),
        )
        .await;
        assert_eq!(renamed["device"]["name"], "tablet");
        assert_eq!(renamed["device"]["enrollment"], "pending");
        for v in [&list, &renamed] {
            let s = v.to_string();
            assert!(!s.contains(&encoded), "secret leaked: {s}");
            assert!(!s.contains("secret"), "secret field leaked: {s}");
        }
        assert_eq!(list["box_fingerprint"], fp);
        assert_eq!(list["devices"].as_array().unwrap().len(), 1);
        assert_eq!(list["devices"][0]["status"]["state"], "offline");

        // A device paired before v2 (no enrollment row) is legacy.
        state
            .db
            .insert_remote_device(NewRemoteDevice {
                id: "old".into(),
                user_id: "u1".into(),
                name: "old phone".into(),
                secret_ciphertext: vec![0; 48],
                secret_nonce: vec![0; 12],
                created_at: "2026-01-01T00:00:00Z".into(),
                last_connected_at: None,
            })
            .await
            .unwrap();
        let (_, list) = call(&state, &token, "GET", "/api/remote-access", None).await;
        let old = list["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["id"] == "old")
            .unwrap();
        assert_eq!(old["enrollment"], "legacy");

        // Revoke deletes the row (and the sealed secret with it).
        let (status, _) = call(
            &state,
            &token,
            "DELETE",
            &format!("/api/remote-access/devices/{id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(state.db.get_remote_device(&id).await.unwrap().is_none());
        assert!(
            state
                .db
                .get_remote_device_enrollment(&id)
                .await
                .unwrap()
                .is_none()
        );
        let (status, _) = call(
            &state,
            &token,
            "DELETE",
            &format!("/api/remote-access/devices/{id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// `…/link` re-issues a pending (or expired) link with a new secret;
    /// once a device enrolled, or for a legacy pairing, it answers 409.
    #[tokio::test]
    async fn link_reissue_only_for_unused_links() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "admin").await;
        let (_, created) = call(
            &state,
            &token,
            "POST",
            "/api/remote-access/devices",
            Some(serde_json::json!({ "name": "phone" })),
        )
        .await;
        let id = created["device"]["id"].as_str().unwrap().to_string();
        let first = link_secret_b64(created["pairing_link"].as_str().unwrap());

        let (status, body) = call(
            &state,
            &token,
            "POST",
            &format!("/api/remote-access/devices/{id}/link"),
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let second = link_secret_b64(body["pairing_link"].as_str().unwrap());
        assert_ne!(first, second, "a new secret");
        assert_eq!(body["device"]["enrollment"], "pending");
        assert_eq!(body["box_fingerprint"], created["box_fingerprint"]);
        let row = state.db.get_remote_device(&id).await.unwrap().unwrap();
        let opened = secret::open(state.remote_access.vault_key(), &row).unwrap();
        use base64::Engine as _;
        assert_eq!(
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(opened.as_bytes()),
            second,
            "the old secret is gone"
        );

        // Enrolled (staged): no re-issue.
        let outcome = state
            .db
            .enroll_remote_device(EnrollAttempt {
                device_id: id.clone(),
                legacy_upgrade: false,
                device_pubkey: [7; 32],
                rendezvous_ciphertext: vec![1; 48],
                rendezvous_nonce: vec![1; 12],
                device_name_hint: "iPhone".into(),
                from: "203.0.113.7:4000".into(),
                now: chrono::Utc::now().to_rfc3339(),
            })
            .await
            .unwrap();
        assert_eq!(outcome, EnrollOutcome::Granted);
        let (status, _) = call(
            &state,
            &token,
            "POST",
            &format!("/api/remote-access/devices/{id}/link"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (_, list) = call(&state, &token, "GET", "/api/remote-access", None).await;
        let d = &list["devices"][0];
        assert_eq!(d["enrollment"], "staged");
        assert_eq!(d["enrolled_from"], "203.0.113.7:4000");
        assert_eq!(d["device_name_hint"], "iPhone");
        assert!(!list.to_string().contains(&second), "no secret in the list");

        // Legacy: no re-issue either; unknown: 404.
        state
            .db
            .insert_remote_device(NewRemoteDevice {
                id: "old".into(),
                user_id: "u1".into(),
                name: "old phone".into(),
                secret_ciphertext: vec![0; 48],
                secret_nonce: vec![0; 12],
                created_at: "2026-01-01T00:00:00Z".into(),
                last_connected_at: None,
            })
            .await
            .unwrap();
        let (status, _) = call(
            &state,
            &token,
            "POST",
            "/api/remote-access/devices/old/link",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, _) = call(
            &state,
            &token,
            "POST",
            "/api/remote-access/devices/nope/link",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn settings_validate_relay_host() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "admin").await;
        let (status, _) = call(
            &state,
            &token,
            "PUT",
            "/api/remote-access",
            Some(serde_json::json!({ "relay_host": "evil host/x" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, body) = call(
            &state,
            &token,
            "PUT",
            "/api/remote-access",
            Some(serde_json::json!({ "enabled": true, "relay_host": "127.0.0.1:24430" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["enabled"], true);
        assert_eq!(body["relay_host"], "127.0.0.1:24430");
    }

    #[tokio::test]
    async fn direct_connection_settings_are_admin_only_and_validated() {
        let direct = |base: serde_json::Value, count: i64, public: &str| {
            Some(serde_json::json!({ "direct": {
                "udp_port_base": base, "udp_port_count": count, "public_address": public,
            }}))
        };
        let user_dir = tempfile::tempdir().unwrap();
        let user_state = test_state(user_dir.path());
        let user = seed_authenticated_user(&user_state, "user").await;
        let (status, _) = call(
            &user_state,
            &user,
            "PUT",
            "/api/remote-access",
            direct(40000.into(), 10, ""),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let admin = seed_authenticated_user(&state, "admin").await;

        for (body, field) in [
            (direct(80.into(), 10, ""), "udp_port_base"),
            (direct(65530.into(), 10, ""), "udp_port_count"),
            (direct(40000.into(), 10, "1.2.3.4:80"), "public_address"),
            (
                direct(serde_json::Value::Null, 10, "1.2.3.4"),
                "public_address",
            ),
        ] {
            let (status, resp) = call(&state, &admin, "PUT", "/api/remote-access", body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(resp["field"], field, "{resp}");
        }
        let (_, body) = call(&state, &admin, "GET", "/api/remote-access", None).await;
        assert_eq!(
            body["udp_port_base"],
            serde_json::Value::Null,
            "nothing saved"
        );
        assert_eq!(body["udp_port_count"], 10);

        let (status, _) = call(
            &state,
            &admin,
            "PUT",
            "/api/remote-access",
            direct(40000.into(), 4, " home.example.com "),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, body) = call(&state, &admin, "GET", "/api/remote-access", None).await;
        assert_eq!(body["udp_port_base"], 40000);
        assert_eq!(body["udp_port_count"], 4);
        assert_eq!(body["public_address"], "home.example.com");
        assert_eq!(body["enabled"], false, "other settings untouched");
    }

    /// The overview carries the box's relay registration; the refresh
    /// endpoint creates the identity on first use, asks the relay now, and
    /// flips to registered once the box's key is in the relay's registry.
    #[tokio::test]
    async fn registration_in_overview_and_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::in_memory().unwrap();
        let backend = Arc::new(FakeBackend::default());
        let ra = RemoteAccess::new(db.clone(), vec![0u8; 32], dir.path(), backend.clone());
        let state = test_state_with(dir.path(), db, ra);
        let token = seed_authenticated_user(&state, "admin").await;

        // Never enabled: no identity, nothing to report.
        let (_, body) = call(&state, &token, "GET", "/api/remote-access", None).await;
        assert_eq!(
            body["registration"],
            serde_json::json!({ "supported": false, "registered": null, "gated": null, "url": "" })
        );
        assert!(!dir.path().join(IDENTITY_FILE).exists());

        // Refresh: the identity exists now and the relay says unregistered
        // (the gate is only known from a box handshake).
        let (status, reg) = call(
            &state,
            &token,
            "POST",
            "/api/remote-access/registration/refresh",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(reg["supported"], true);
        assert_eq!(reg["registered"], false);
        assert_eq!(reg["gated"], serde_json::Value::Null);
        let url = reg["url"].as_str().unwrap();
        assert!(
            url.starts_with("https://relay.peckboard.com/register#") && url.len() > 44,
            "{url}"
        );
        assert!(dir.path().join(IDENTITY_FILE).exists());
        let (_, body) = call(&state, &token, "GET", "/api/remote-access", None).await;
        assert_eq!(body["registration"], reg);

        // The admin registered the key on the relay.
        backend.registered.store(true, SeqCst);
        state.remote_access.forget_last_poll();
        let (_, reg) = call(
            &state,
            &token,
            "POST",
            "/api/remote-access/registration/refresh",
            None,
        )
        .await;
        assert_eq!(reg["registered"], true);
        assert_eq!(reg["url"], url);
        let (_, body) = call(&state, &token, "GET", "/api/remote-access", None).await;
        assert_eq!(body["registration"]["registered"], true);
    }
}
