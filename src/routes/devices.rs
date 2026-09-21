//! `/api/devices/*` — enrollment surface for remote-control machines (the
//! peckboard-agent daemon). Every route is JWT-authenticated
//! (`require_auth`) and scoped to the caller's own devices; an admin may
//! additionally revoke any device (mirrors `routes/auth.rs`'s session
//! revoke).
//!
//! The enrollment token is a 32-byte random value handed back to the
//! caller EXACTLY ONCE at creation. Only its SHA-256 hex
//! (`crate::auth::token::hash_token`) is stored in `devices.secret_hash`;
//! the plaintext is never persisted and never logged. `DeviceView` — the
//! only shape ever serialized into a response after creation — omits
//! `secret_hash` entirely.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};

use crate::auth::middleware::{AuthUser, require_auth};
use crate::auth::token::hash_token;
use crate::db::models::{Device, NewDevice, device_status};
use crate::state::AppState;
use crate::ws::agent::DeviceRegistry;

const NAME_MAX_LEN: usize = 128;
const PLATFORM_MAX_LEN: usize = 64;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/devices", get(list).post(enroll))
        .route(
            "/api/devices/{id}",
            axum::routing::delete(revoke).patch(update),
        )
        .route("/api/devices/{id}/activity", get(activity))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}

fn internal_err(e: impl std::fmt::Display) -> Response {
    err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
}

/// Response shape for a device — public metadata only. `secret_hash`
/// never leaves this module. `online` / `in_flight` are the
/// [`DeviceRegistry`]'s live view (socket connected, replies pending),
/// not DB columns — included so the Agents panel's first render doesn't
/// need to wait for a `device-update` frame.
#[derive(Serialize)]
struct DeviceView {
    id: String,
    name: String,
    platform: String,
    status: String,
    last_seen_at: Option<String>,
    created_at: String,
    online: bool,
    in_flight: usize,
}

impl DeviceView {
    fn of(d: &Device, registry: &DeviceRegistry) -> Self {
        DeviceView {
            id: d.id.clone(),
            name: d.name.clone(),
            platform: d.platform.clone(),
            status: d.status.clone(),
            last_seen_at: d.last_seen_at.clone(),
            created_at: d.created_at.clone(),
            online: registry.is_online(&d.id),
            in_flight: registry.in_flight(&d.id),
        }
    }
}

/// GET /api/devices — the caller's own devices, newest first.
async fn list(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
) -> Response {
    match state.db.list_devices_by_user(&user.user_id).await {
        Ok(devices) => {
            let views: Vec<DeviceView> = devices
                .iter()
                .map(|d| DeviceView::of(d, &state.device_registry))
                .collect();
            Json(serde_json::json!({ "devices": views })).into_response()
        }
        Err(e) => internal_err(e),
    }
}

#[derive(Deserialize)]
struct EnrollBody {
    name: String,
    platform: String,
}

/// POST /api/devices — enroll a new device. Mints a 32-byte random
/// enrollment token, stores only its SHA-256 hash, and returns the
/// plaintext token ONCE (it can never be recovered afterwards).
async fn enroll(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Json(body): Json<EnrollBody>,
) -> Response {
    let name = body.name.trim().to_string();
    let platform = body.platform.trim().to_string();
    if name.is_empty() || name.len() > NAME_MAX_LEN {
        return err(StatusCode::BAD_REQUEST, "name must be 1..=128 chars");
    }
    if platform.is_empty() || platform.len() > PLATFORM_MAX_LEN {
        return err(StatusCode::BAD_REQUEST, "platform must be 1..=64 chars");
    }

    // 32 random bytes, hex-encoded → the plaintext enrollment token. Only
    // its hash is stored; this is the sole moment the plaintext exists.
    let token = {
        use rand::Rng;
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill(&mut bytes[..]);
        hex::encode(bytes)
    };
    let secret_hash = hash_token(&token);

    let new = NewDevice {
        id: uuid::Uuid::new_v4().to_string(),
        user_id: user.user_id.clone(),
        name,
        platform,
        secret_hash,
        status: device_status::ACTIVE.to_string(),
        last_seen_at: None,
        created_at: chrono::Utc::now().to_rfc3339(),
    };

    match state.db.insert_device(new).await {
        Ok(device) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "device": DeviceView::of(&device, &state.device_registry),
                "enrollment_token": token,
            })),
        )
            .into_response(),
        Err(e) => internal_err(e),
    }
}

#[derive(Deserialize)]
struct UpdateBody {
    name: Option<String>,
    status: Option<String>,
}

/// PATCH /api/devices/:id — rename and/or flip the kill-switch (owner or
/// admin). `status` may only move between `active` and `disabled` here;
/// `revoked` is DELETE's job and a revoked device can never be
/// resurrected. Unknown ids, other users' devices, and revoked devices
/// all return 404 (no existence leak).
async fn update(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<UpdateBody>,
) -> Response {
    let device = match state.db.get_device(&id).await {
        Ok(Some(d)) => d,
        Ok(None) => return err(StatusCode::NOT_FOUND, "no such device"),
        Err(e) => return internal_err(e),
    };
    if (device.user_id != user.user_id && !user.is_admin())
        || device.status == device_status::REVOKED
    {
        return err(StatusCode::NOT_FOUND, "no such device");
    }

    let name = match &body.name {
        Some(raw) => {
            let name = raw.trim().to_string();
            if name.is_empty() || name.len() > NAME_MAX_LEN {
                return err(StatusCode::BAD_REQUEST, "name must be 1..=128 chars");
            }
            Some(name)
        }
        None => None,
    };
    let status = match body.status.as_deref() {
        None => None,
        Some(s) if s == device_status::ACTIVE || s == device_status::DISABLED => Some(s),
        Some(_) => {
            return err(
                StatusCode::BAD_REQUEST,
                "status must be 'active' or 'disabled'",
            );
        }
    };

    if let Some(name) = &name
        && let Err(e) = state.db.update_device_name(&id, name).await
    {
        return internal_err(e);
    }
    if let Some(status) = status
        && let Err(e) = state.db.update_device_status(&id, status).await
    {
        return internal_err(e);
    }

    // Disabling cuts the live socket NOW (the socket task's 10s DB
    // re-check is only the backstop); the broadcast makes open Agents
    // panels refetch the changed row immediately.
    if status == Some(device_status::DISABLED) {
        state.device_registry.sever(&id);
    }
    state
        .device_registry
        .broadcast_state(&state.broadcaster, &id);

    match state.db.get_device(&id).await {
        Ok(Some(d)) => Json(serde_json::json!({
            "device": DeviceView::of(&d, &state.device_registry),
        }))
        .into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, "no such device"),
        Err(e) => internal_err(e),
    }
}

/// DELETE /api/devices/:id — revoke a device (owner or admin). Sets
/// `status=revoked` rather than deleting the row, so the audit trail and
/// the unique `secret_hash` are preserved. Unknown ids and other users'
/// devices both return 404 (no existence leak).
async fn revoke(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Response {
    let device = match state.db.get_device(&id).await {
        Ok(Some(d)) => d,
        Ok(None) => return err(StatusCode::NOT_FOUND, "no such device"),
        Err(e) => return internal_err(e),
    };
    if device.user_id != user.user_id && !user.is_admin() {
        return err(StatusCode::NOT_FOUND, "no such device");
    }

    // Cut the live socket NOW (the 10s /ws/agent DB re-check is only the
    // backstop); the handler refuses revoked devices on reconnect. The
    // broadcast makes open Agents panels refetch now rather than on the
    // next frame.
    match state
        .db
        .update_device_status(&id, device_status::REVOKED)
        .await
    {
        Ok(_) => {
            state.device_registry.sever(&id);
            state
                .device_registry
                .broadcast_state(&state.broadcaster, &id);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => internal_err(e),
    }
}

/// How many audit rows the activity endpoint returns.
const ACTIVITY_LIMIT: i64 = 50;

/// GET /api/devices/:id/activity — the device's audit log of bridged
/// remote-agent actions (written by the `remote_agent_*` MCP handlers),
/// newest first, capped at [`ACTIVITY_LIMIT`]. Owner or admin only;
/// unknown ids, other users' devices, and revoked devices all return 404
/// (same gate as PATCH — no existence leak).
async fn activity(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> Response {
    let device = match state.db.get_device(&id).await {
        Ok(Some(d)) => d,
        Ok(None) => return err(StatusCode::NOT_FOUND, "no such device"),
        Err(e) => return internal_err(e),
    };
    if (device.user_id != user.user_id && !user.is_admin())
        || device.status == device_status::REVOKED
    {
        return err(StatusCode::NOT_FOUND, "no such device");
    }
    match state.db.list_device_activity(&id, ACTIVITY_LIMIT).await {
        Ok(rows) => Json(serde_json::json!({ "activity": rows })).into_response(),
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

    fn app(state: Arc<AppState>) -> Router {
        router(state.clone()).with_state(state)
    }

    fn req(
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> Request<Body> {
        let mut b = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let body = match body {
            Some(v) => {
                b = b.header(header::CONTENT_TYPE, "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        b.body(body).unwrap()
    }

    async fn json(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn enroll_hash_verify_list_revoke() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());
        let token = seed_authenticated_user(&state, "user").await;

        // enroll
        let resp = app(state.clone())
            .oneshot(req(
                "POST",
                "/api/devices",
                Some(&token),
                Some(serde_json::json!({ "name": "  laptop  ", "platform": "linux" })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = json(resp).await;
        let enroll_token = body["enrollment_token"].as_str().unwrap().to_string();
        assert_eq!(enroll_token.len(), 64, "32 bytes hex-encoded");
        assert_eq!(body["device"]["name"], "laptop", "name trimmed");
        assert_eq!(body["device"]["status"], device_status::ACTIVE);
        assert!(
            body["device"].get("secret_hash").is_none(),
            "hash never serialized"
        );
        let device_id = body["device"]["id"].as_str().unwrap().to_string();

        // DB stores only the hash, never the plaintext token
        let row = state.db.get_device(&device_id).await.unwrap().unwrap();
        assert_eq!(row.secret_hash, hash_token(&enroll_token));
        assert_ne!(row.secret_hash, enroll_token);
        // and the presented token round-trips through the by-hash lookup
        let by_hash = state
            .db
            .get_device_by_secret_hash(&hash_token(&enroll_token))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(by_hash.id, device_id);

        // list shows the device, no secret_hash
        let resp = app(state.clone())
            .oneshot(req("GET", "/api/devices", Some(&token), None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json(resp).await;
        let devices = body["devices"].as_array().unwrap();
        assert_eq!(devices.len(), 1);
        assert!(devices[0].get("secret_hash").is_none());

        // revoke
        let resp = app(state.clone())
            .oneshot(req(
                "DELETE",
                &format!("/api/devices/{device_id}"),
                Some(&token),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let row = state.db.get_device(&device_id).await.unwrap().unwrap();
        assert_eq!(row.status, device_status::REVOKED);
    }

    #[tokio::test]
    async fn no_auth_is_401() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());
        let resp = app(state.clone())
            .oneshot(req("GET", "/api/devices", None, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn non_owner_revoke_is_404() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());
        let owner = seed_authenticated_user(&state, "user").await;
        let other = crate::auth::middleware::tests::seed_authenticated_user_with_suffix(
            &state, "user", "b",
        )
        .await;

        // owner enrolls a device
        let resp = app(state.clone())
            .oneshot(req(
                "POST",
                "/api/devices",
                Some(&owner),
                Some(serde_json::json!({ "name": "laptop", "platform": "linux" })),
            ))
            .await
            .unwrap();
        let device_id = json(resp).await["device"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        // a different user cannot revoke it — 404, and the row is untouched
        let resp = app(state.clone())
            .oneshot(req(
                "DELETE",
                &format!("/api/devices/{device_id}"),
                Some(&other),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let row = state.db.get_device(&device_id).await.unwrap().unwrap();
        assert_eq!(row.status, device_status::ACTIVE);
    }

    #[tokio::test]
    async fn patch_renames_and_toggles_kill_switch() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());
        let token = seed_authenticated_user(&state, "user").await;

        let resp = app(state.clone())
            .oneshot(req(
                "POST",
                "/api/devices",
                Some(&token),
                Some(serde_json::json!({ "name": "laptop", "platform": "linux" })),
            ))
            .await
            .unwrap();
        let device_id = json(resp).await["device"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        // rename + disable in one PATCH
        let resp = app(state.clone())
            .oneshot(req(
                "PATCH",
                &format!("/api/devices/{device_id}"),
                Some(&token),
                Some(serde_json::json!({ "name": "  desk box  ", "status": "disabled" })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json(resp).await;
        assert_eq!(body["device"]["name"], "desk box", "name trimmed");
        assert_eq!(body["device"]["status"], device_status::DISABLED);
        assert_eq!(body["device"]["online"], false);
        assert_eq!(body["device"]["in_flight"], 0);

        // re-enable
        let resp = app(state.clone())
            .oneshot(req(
                "PATCH",
                &format!("/api/devices/{device_id}"),
                Some(&token),
                Some(serde_json::json!({ "status": "active" })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json(resp).await["device"]["status"], device_status::ACTIVE);

        // bad status is a 400 and changes nothing
        let resp = app(state.clone())
            .oneshot(req(
                "PATCH",
                &format!("/api/devices/{device_id}"),
                Some(&token),
                Some(serde_json::json!({ "status": "revoked" })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let row = state.db.get_device(&device_id).await.unwrap().unwrap();
        assert_eq!(row.status, device_status::ACTIVE);
    }

    #[tokio::test]
    async fn non_owner_patch_is_404_and_revoked_is_immutable() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());
        let owner = seed_authenticated_user(&state, "user").await;
        let other = crate::auth::middleware::tests::seed_authenticated_user_with_suffix(
            &state, "user", "b",
        )
        .await;

        let resp = app(state.clone())
            .oneshot(req(
                "POST",
                "/api/devices",
                Some(&owner),
                Some(serde_json::json!({ "name": "laptop", "platform": "linux" })),
            ))
            .await
            .unwrap();
        let device_id = json(resp).await["device"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        let resp = app(state.clone())
            .oneshot(req(
                "PATCH",
                &format!("/api/devices/{device_id}"),
                Some(&other),
                Some(serde_json::json!({ "name": "hijacked" })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let row = state.db.get_device(&device_id).await.unwrap().unwrap();
        assert_eq!(row.name, "laptop");

        // revoke, then PATCH can't resurrect it
        let resp = app(state.clone())
            .oneshot(req(
                "DELETE",
                &format!("/api/devices/{device_id}"),
                Some(&owner),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let resp = app(state.clone())
            .oneshot(req(
                "PATCH",
                &format!("/api/devices/{device_id}"),
                Some(&owner),
                Some(serde_json::json!({ "status": "active" })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn activity_owner_reads_newest_first_non_owner_404() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());
        let owner = seed_authenticated_user(&state, "user").await;
        let other = crate::auth::middleware::tests::seed_authenticated_user_with_suffix(
            &state, "user", "b",
        )
        .await;

        let resp = app(state.clone())
            .oneshot(req(
                "POST",
                "/api/devices",
                Some(&owner),
                Some(serde_json::json!({ "name": "laptop", "platform": "linux" })),
            ))
            .await
            .unwrap();
        let device_id = json(resp).await["device"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        for (i, status) in ["ok", "timeout"].iter().enumerate() {
            state
                .db
                .insert_device_activity(crate::db::models::NewDeviceActivity {
                    id: format!("a{i}"),
                    device_id: device_id.clone(),
                    session_id: Some("s1".into()),
                    capability: "echo".into(),
                    args_summary: "{\"text\":\"hi\"}".into(),
                    status: status.to_string(),
                    created_at: format!("2026-01-0{}T00:00:00Z", i + 1),
                })
                .await
                .unwrap();
        }

        // owner: newest first
        let resp = app(state.clone())
            .oneshot(req(
                "GET",
                &format!("/api/devices/{device_id}/activity"),
                Some(&owner),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let rows = json(resp).await["activity"].as_array().unwrap().to_vec();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["status"], "timeout");
        assert_eq!(rows[1]["status"], "ok");
        assert_eq!(rows[0]["capability"], "echo");
        assert_eq!(rows[0]["session_id"], "s1");

        // non-owner: 404, no existence leak
        let resp = app(state.clone())
            .oneshot(req(
                "GET",
                &format!("/api/devices/{device_id}/activity"),
                Some(&other),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // revoked device: 404
        app(state.clone())
            .oneshot(req(
                "DELETE",
                &format!("/api/devices/{device_id}"),
                Some(&owner),
                None,
            ))
            .await
            .unwrap();
        let resp = app(state.clone())
            .oneshot(req(
                "GET",
                &format!("/api/devices/{device_id}/activity"),
                Some(&owner),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// Revoke / disable must cut a live socket immediately — not wait for
    /// the socket task's 10s DB re-check.
    #[tokio::test]
    async fn revoke_and_disable_sever_the_live_socket_immediately() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());
        let token = seed_authenticated_user(&state, "user").await;

        for (method, body) in [
            ("DELETE", None),
            ("PATCH", Some(serde_json::json!({ "status": "disabled" }))),
        ] {
            let resp = app(state.clone())
                .oneshot(req(
                    "POST",
                    "/api/devices",
                    Some(&token),
                    Some(serde_json::json!({ "name": "box", "platform": "linux" })),
                ))
                .await
                .unwrap();
            let device_id = json(resp).await["device"]["id"]
                .as_str()
                .unwrap()
                .to_string();

            // Fake live daemon connection.
            let mut conn = state.device_registry.connect(&device_id);
            assert!(state.device_registry.is_online(&device_id));

            let resp = app(state.clone())
                .oneshot(req(
                    method,
                    &format!("/api/devices/{device_id}"),
                    Some(&token),
                    body,
                ))
                .await
                .unwrap();
            assert!(resp.status().is_success(), "{method} failed");

            assert!(
                !state.device_registry.is_online(&device_id),
                "{method} must sever the live socket immediately"
            );
            // The socket task's outbound queue ends (sender dropped).
            assert!(conn.outbound.recv().await.is_none());
        }
    }
}
