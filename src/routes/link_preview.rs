//! Link previews (`service::link_preview`).
//!
//! - `GET /api/link-preview?url=…` — JWT (any logged-in user). Preview JSON;
//!   image/favicon fields are signed `/api/link-preview/image` URLs.
//! - `GET /api/link-preview/image?k&u&exp&sig` — NO JWT: the HMAC over
//!   `(k, u, exp)` minted by the preview route is the credential, so a plain
//!   `<img>` works. Re-applies the SSRF guard and serves sniffed raster
//!   bytes only.
//! - `GET|PUT /api/settings/link-preview` — admin: trusted internal hosts +
//!   the screenshot fallback switch.

use crate::auth::middleware::{AuthUser, require_admin, require_auth};
use crate::service::link_preview::{
    self, ImageKind, LinkPreviewSettings, PreviewError, internal::InstanceHosts,
};
use crate::service::shared_files::now_secs;
use crate::state::AppState;
use axum::{
    Extension, Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use std::sync::Arc;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let authed = Router::new()
        .route("/api/link-preview", get(get_preview))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));
    let admin = Router::new()
        .route(
            "/api/settings/link-preview",
            get(get_settings).put(put_settings),
        )
        .route_layer(middleware::from_fn(require_admin))
        .route_layer(middleware::from_fn_with_state(state, require_auth));
    Router::new()
        .route("/api/link-preview/image", get(get_image))
        .merge(authed)
        .merge(admin)
}

fn error_response(e: PreviewError) -> Response {
    let status = StatusCode::from_u16(e.status).unwrap_or(StatusCode::BAD_GATEWAY);
    (status, Json(serde_json::json!({ "error": e.message }))).into_response()
}

#[derive(Deserialize)]
struct PreviewQuery {
    url: String,
}

/// GET /api/link-preview?url=<http(s) url>
async fn get_preview(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<AuthUser>,
    headers: HeaderMap,
    Query(q): Query<PreviewQuery>,
) -> Response {
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let hosts = InstanceHosts::new(host, &[state.config.port, state.config.https_port]);
    match link_preview::service()
        .preview(&state.db, &q.url, &hosts, user.is_admin())
        .await
    {
        Ok(p) => Json(p.for_client(&state.jwt_secret, now_secs())).into_response(),
        Err(e) => error_response(e),
    }
}

#[derive(Deserialize)]
struct ImageQuery {
    k: String,
    u: String,
    exp: u64,
    sig: String,
}

/// GET /api/link-preview/image?k=img|shot&u=<url>&exp=<unix>&sig=<hex>
async fn get_image(State(state): State<Arc<AppState>>, Query(q): Query<ImageQuery>) -> Response {
    let Some(kind) = ImageKind::parse(&q.k) else {
        return error_response(PreviewError {
            status: 400,
            message: "bad image kind".into(),
        });
    };
    if !link_preview::verify_image_sig(&state.jwt_secret, kind, &q.u, q.exp, &q.sig, now_secs()) {
        return error_response(PreviewError {
            status: 403,
            message: "invalid or expired signature".into(),
        });
    }
    match link_preview::service().image(&state.db, kind, &q.u).await {
        Ok(img) => (
            [
                (header::CONTENT_TYPE, img.mime.to_string()),
                (header::CACHE_CONTROL, "private, max-age=21600".to_string()),
                (
                    header::HeaderName::from_static("x-content-type-options"),
                    "nosniff".to_string(),
                ),
                (
                    header::HeaderName::from_static("content-security-policy"),
                    "default-src 'none'; sandbox".to_string(),
                ),
            ],
            img.bytes,
        )
            .into_response(),
        Err(e) => error_response(e),
    }
}

/// GET /api/settings/link-preview →
/// `{"trusted_internal_hosts": [..], "screenshot_fallback": bool}`.
async fn get_settings(State(state): State<Arc<AppState>>) -> Response {
    Json(link_preview::load_settings(&state.db).await).into_response()
}

/// PUT /api/settings/link-preview (same shape) → 204.
async fn put_settings(
    State(state): State<Arc<AppState>>,
    Json(body): Json<LinkPreviewSettings>,
) -> Response {
    if let Err(msg) = body.validate() {
        return error_response(PreviewError {
            status: 400,
            message: msg,
        });
    }
    let normalised = LinkPreviewSettings {
        trusted_internal_hosts: body
            .trusted_internal_hosts
            .iter()
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty())
            .collect(),
        ..body
    };
    match link_preview::save_settings(&state.db, &normalised).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => error_response(PreviewError {
            status: 500,
            message: e.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::middleware::tests::{seed_authenticated_user, test_state};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn call(
        state: &Arc<AppState>,
        token: Option<&str>,
        method: &str,
        uri: &str,
        body: &str,
    ) -> (StatusCode, serde_json::Value) {
        let app = router(state.clone()).with_state(state.clone());
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "board.lan:3344")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(t) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let resp = app
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn preview_requires_auth_and_classifies_internal_links() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let port = state.config.port;
        let uri = format!(
            "/api/link-preview?url={}",
            urlencoding::encode(&format!("http://board.lan:{port}/projects/p1/cards/c2"))
        );
        let (status, _) = call(&state, None, "GET", &uri, "").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let token = seed_authenticated_user(&state, "user").await;
        // The Host header names the instance; use its port too.
        let uri =
            "/api/link-preview?url=http%3A%2F%2Fboard.lan%3A3344%2Fprojects%2Fp1%2Fcards%2Fc2";
        let (status, body) = call(&state, Some(&token), "GET", uri, "").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["kind"], "internal");
        assert_eq!(body["internal"]["entity"], "card");
        assert_eq!(body["internal"]["id"], "c2");
        assert_eq!(body["internal"]["project_id"], "p1");

        let (status, _) = call(
            &state,
            Some(&token),
            "GET",
            "/api/link-preview?url=file%3A%2F%2F%2Fetc%2Fpasswd",
            "",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn settings_are_admin_only_and_validated() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let user = seed_authenticated_user(&state, "user").await;
        let (status, _) = call(&state, Some(&user), "GET", "/api/settings/link-preview", "").await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let admin = crate::auth::middleware::tests::seed_authenticated_user_with_suffix(
            &state, "admin", "lp",
        )
        .await;
        let (status, body) = call(
            &state,
            Some(&admin),
            "GET",
            "/api/settings/link-preview",
            "",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            serde_json::json!({"trusted_internal_hosts": [], "screenshot_fallback": false})
        );

        let (status, _) = call(
            &state,
            Some(&admin),
            "PUT",
            "/api/settings/link-preview",
            r#"{"trusted_internal_hosts": ["not a host!"]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = call(&state, Some(&admin), "PUT", "/api/settings/link-preview",
            r#"{"trusted_internal_hosts": [" nas.lan ", "10.0.0.0/8"], "screenshot_fallback": true}"#).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, body) = call(
            &state,
            Some(&admin),
            "GET",
            "/api/settings/link-preview",
            "",
        )
        .await;
        assert_eq!(
            body["trusted_internal_hosts"],
            serde_json::json!(["nas.lan", "10.0.0.0/8"])
        );
        assert_eq!(body["screenshot_fallback"], true);
    }

    #[tokio::test]
    async fn image_proxy_rejects_bad_signatures() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let (status, _) = call(
            &state,
            None,
            "GET",
            "/api/link-preview/image?k=img&u=http%3A%2F%2F127.0.0.1%2F&exp=99999999999&sig=00",
            "",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
}
