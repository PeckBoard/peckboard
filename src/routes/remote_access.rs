//! `/api/remote-access/*` — remote access through the relay. Admin-only:
//! the setting opens a path into this host from the internet, and every
//! paired device gets the same reach as the admin's browser.
//!
//! The pairing secret is minted by `POST /api/remote-access/devices`
//! and handed back EXACTLY ONCE, inside the pairing link (+ its QR code).
//! It is stored only sealed (`service::remote_access::secret`) and no
//! response shape after creation carries it — `DeviceView` is built field
//! by field and `RemoteDevice` isn't even `Serialize`.

use std::sync::Arc;

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
use crate::db::models::{NewRemoteDevice, RemoteDevice};
use crate::service::remote_access::{
    DeviceStatus, RemoteAccess, RemoteAccessSettings, secret, validate_direct, validate_relay_host,
};
use crate::state::AppState;

const NAME_MAX_LEN: usize = 128;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/remote-access", get(overview).put(put_settings))
        .route("/api/remote-access/devices", post(create))
        .route(
            "/api/remote-access/devices/{id}",
            patch(rename).delete(revoke),
        )
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

/// Public metadata + live tunnel status. Never the secret.
#[derive(Serialize)]
struct DeviceView {
    id: String,
    name: String,
    created_at: String,
    last_connected_at: Option<String>,
    status: DeviceStatus,
}

impl DeviceView {
    fn of(d: &RemoteDevice, ra: &RemoteAccess) -> Self {
        DeviceView {
            id: d.id.clone(),
            name: d.name.clone(),
            created_at: d.created_at.clone(),
            last_connected_at: d.last_connected_at.clone(),
            status: ra.status(&d.id),
        }
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
/// `udp_port_base`, `udp_port_count`, `public_address`) + `devices`.
async fn overview(State(state): State<Arc<AppState>>) -> Response {
    let ra = &state.remote_access;
    let settings = ra.settings().await;
    match state.db.list_remote_devices().await {
        Ok(devices) => {
            let views: Vec<DeviceView> = devices.iter().map(|d| DeviceView::of(d, ra)).collect();
            let mut body = settings_json(&settings);
            body.insert("devices".into(), serde_json::json!(views));
            Json(body).into_response()
        }
        Err(e) => internal_err(e),
    }
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

/// POST /api/remote-access/devices `{name}` → 201
/// `{device, pairing_link, qr_svg}`. The only response that ever carries
/// the secret.
async fn create(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Json(body): Json<NameBody>,
) -> Response {
    let Some(name) = valid_name(&body.name) else {
        return err(StatusCode::BAD_REQUEST, "name must be 1..=128 chars");
    };
    let id = uuid::Uuid::new_v4().to_string();
    let s = secret::DeviceSecret::generate();
    let (secret_ciphertext, secret_nonce) =
        match secret::seal(state.remote_access.vault_key(), &id, &s) {
            Ok(v) => v,
            Err(e) => return internal_err(e),
        };
    let row = match state
        .db
        .insert_remote_device(NewRemoteDevice {
            id,
            user_id: user.user_id.clone(),
            name,
            secret_ciphertext,
            secret_nonce,
            created_at: chrono::Utc::now().to_rfc3339(),
            last_connected_at: None,
        })
        .await
    {
        Ok(r) => r,
        Err(e) => return internal_err(e),
    };
    state.remote_access.reconcile().await;

    let relay_host = state.remote_access.settings().await.relay_host;
    let link = s.pairing_link(&relay_host);
    let qr_svg = QrCode::new(link.as_bytes())
        .map(|c| {
            c.render::<qrcode::render::svg::Color>()
                .min_dimensions(200, 200)
                .build()
        })
        .unwrap_or_default();
    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "device": DeviceView::of(&row, &state.remote_access),
            "pairing_link": link,
            "qr_svg": qr_svg,
        })),
    )
        .into_response()
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
        Ok(Some(d)) => {
            Json(serde_json::json!({ "device": DeviceView::of(&d, &state.remote_access) }))
                .into_response()
        }
        Ok(None) => err(StatusCode::NOT_FOUND, "no such device"),
        Err(e) => internal_err(e),
    }
}

/// DELETE /api/remote-access/devices/:id → 204. Deletes the row (and so
/// the sealed secret — the device's rendezvous id dies with it) and drops
/// the live tunnel immediately.
async fn revoke(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match state.db.delete_remote_device(&id).await {
        Ok(true) => {
            state.remote_access.stop_device(&id);
            tracing::info!(device_id = %id, "remote access device revoked");
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => err(StatusCode::NOT_FOUND, "no such device"),
        Err(e) => internal_err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::middleware::tests::{seed_authenticated_user, test_state};
    use axum::body::Body;
    use axum::http::{Request, header};
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
            ("DELETE", "/api/remote-access/devices/x", None),
        ] {
            let (status, _) = call(&state, &token, method, uri, body).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
        }
        assert!(state.db.list_remote_devices().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn pair_once_list_without_secret_rename_revoke() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "admin").await;

        let (status, body) = call(&state, &token, "GET", "/api/remote-access", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["enabled"], false, "off by default");
        assert_eq!(body["relay_host"], "relay.peckboard.com");

        let (status, body) = call(
            &state,
            &token,
            "POST",
            "/api/remote-access/devices",
            Some(serde_json::json!({ "name": " phone " })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let link = body["pairing_link"].as_str().unwrap().to_string();
        assert!(link.starts_with("peckboard://pair/"), "{link}");
        assert!(link.ends_with("?relay=relay.peckboard.com"), "{link}");
        assert!(body["qr_svg"].as_str().unwrap().contains("<svg"));
        assert_eq!(body["device"]["name"], "phone");
        let id = body["device"]["id"].as_str().unwrap().to_string();
        let encoded = link
            .trim_start_matches("peckboard://pair/")
            .split('?')
            .next()
            .unwrap()
            .to_string();

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
        for v in [&list, &renamed] {
            let s = v.to_string();
            assert!(!s.contains(&encoded), "secret leaked: {s}");
            assert!(!s.contains("secret"), "secret field leaked: {s}");
        }
        assert_eq!(list["devices"].as_array().unwrap().len(), 1);
        assert_eq!(list["devices"][0]["status"]["state"], "offline");

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
}
