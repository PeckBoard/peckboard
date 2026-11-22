//! `GET /api/pairing/qr` — a QR code of the address a phone's browser can
//! open this instance at, for the first-launch "use it on your phone" card.
//!
//! This is NOT the PeckBoard app's pairing link: the app pairs through the
//! relay with a one-time secret (`POST /api/remote-access/devices`, admin
//! only). This only encodes a plain URL, so any signed-in user may ask.
//!
//! The URL is picked, in order: the page's own origin when it is already
//! HTTPS on a non-loopback host (covers reverse proxies); otherwise, when
//! this server's HTTPS listener is up, `https://<host>:<https_port>` for
//! the request host or — when the browser is on localhost — the first
//! non-loopback name the certificate covers. Plain HTTP or loopback-only
//! answers `reachable: false` with a `reason` (`no_https` | `localhost`),
//! and the UI shows the Settings → Secure connection (HTTPS) guidance.

use std::net::IpAddr;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, Uri, header},
    middleware,
    response::IntoResponse,
    routing::get,
};
use qrcode::QrCode;
use serde::{Deserialize, Serialize};

use crate::auth::middleware::require_auth;
use crate::state::AppState;

/// Longest `origin` accepted; anything longer is ignored (Host is used).
const ORIGIN_MAX_LEN: usize = 2048;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/pairing/qr", get(pairing_qr))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

#[derive(Deserialize)]
struct QrQuery {
    /// The page's `window.location.origin`.
    origin: Option<String>,
}

#[derive(Serialize, Debug, PartialEq)]
struct PairingQr {
    url: Option<String>,
    qr_svg: Option<String>,
    reachable: bool,
    /// `no_https` | `localhost` when not reachable.
    reason: Option<&'static str>,
}

/// `localhost`, `*.localhost`, loopback and unspecified IPs. Brackets of an
/// IPv6 authority are tolerated.
fn is_loopback(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    if h.eq_ignore_ascii_case("localhost") || h.to_ascii_lowercase().ends_with(".localhost") {
        return true;
    }
    h.parse::<IpAddr>()
        .map(|ip| ip.is_loopback() || ip.is_unspecified())
        .unwrap_or(false)
}

fn https_url(host: &str, port: u16) -> String {
    let h = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    if port == 443 || port == 0 {
        format!("https://{h}")
    } else {
        format!("https://{h}:{port}")
    }
}

fn qr_svg(text: &str) -> Option<String> {
    QrCode::new(text.as_bytes()).ok().map(|c| {
        c.render::<qrcode::render::svg::Color>()
            .min_dimensions(200, 200)
            .build()
    })
}

/// Pick the phone-reachable URL (see the module docs).
fn pick_url(
    origin: Option<&str>,
    host_header: Option<&str>,
    https_enabled: bool,
    https_port: u16,
    sans: &[String],
) -> Result<String, &'static str> {
    let parsed: Option<Uri> = origin
        .filter(|o| o.len() <= ORIGIN_MAX_LEN)
        .and_then(|o| o.parse().ok())
        .filter(|u: &Uri| u.host().is_some())
        .or_else(|| {
            host_header
                .filter(|h| h.len() <= ORIGIN_MAX_LEN)
                .and_then(|h| format!("http://{h}").parse().ok())
        });
    let host = parsed.as_ref().and_then(|u| u.host()).map(str::to_string);
    if let (Some(u), Some(h)) = (&parsed, &host)
        && u.scheme_str() == Some("https")
        && !is_loopback(h)
        && let Some(auth) = u.authority()
    {
        return Ok(format!("https://{auth}"));
    }
    if !https_enabled {
        return Err("no_https");
    }
    host.into_iter()
        .chain(sans.iter().cloned())
        .find(|h| !h.is_empty() && !h.starts_with('*') && !is_loopback(h))
        .map(|h| https_url(&h, https_port))
        .ok_or("localhost")
}

/// GET /api/pairing/qr?origin=… → `{url, qr_svg, reachable, reason}`.
async fn pairing_qr(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<QrQuery>,
) -> impl IntoResponse {
    let tls = state.tls.snapshot();
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let body = match pick_url(
        q.origin.as_deref(),
        host,
        tls.https_enabled,
        state.config.https_port,
        &tls.sans,
    ) {
        Ok(url) => PairingQr {
            qr_svg: qr_svg(&url),
            url: Some(url),
            reachable: true,
            reason: None,
        },
        Err(reason) => PairingQr {
            url: None,
            qr_svg: None,
            reachable: false,
            reason: Some(reason),
        },
    };
    Json(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::middleware::tests::{seed_authenticated_user, test_state};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn get_qr(
        state: &Arc<AppState>,
        token: &str,
        uri: &str,
        host: &str,
    ) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .uri(uri)
            .header(header::HOST, host)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let resp = router(state.clone())
            .with_state(state.clone())
            .oneshot(req)
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// An HTTPS page origin is encoded as-is with a QR; a plain-HTTP
    /// localhost page without the HTTPS listener gets the guidance reason;
    /// with HTTPS on, a LAN Host becomes `https://<lan>:<https_port>`.
    #[tokio::test]
    async fn pairing_qr_encodes_the_reachable_url() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let token = seed_authenticated_user(&state, "user").await;

        let (status, body) = get_qr(
            &state,
            &token,
            "/api/pairing/qr?origin=https%3A%2F%2Fbox.example%3A8443",
            "box.example:8443",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["url"], "https://box.example:8443");
        assert_eq!(body["reachable"], true);
        assert!(body["qr_svg"].as_str().unwrap().contains("<svg"));

        let (_, body) = get_qr(
            &state,
            &token,
            "/api/pairing/qr?origin=http%3A%2F%2Flocalhost%3A3344",
            "localhost:3344",
        )
        .await;
        assert_eq!(body["reachable"], false);
        assert_eq!(body["reason"], "no_https");
        assert_eq!(body["qr_svg"], serde_json::Value::Null);

        state.tls.set_https_enabled(true);
        let (_, body) = get_qr(&state, &token, "/api/pairing/qr", "192.168.1.20:3344").await;
        assert_eq!(body["reachable"], true);
        assert_eq!(
            body["url"],
            https_url("192.168.1.20", state.config.https_port)
        );
    }

    #[test]
    fn localhost_with_https_falls_back_to_a_certificate_name() {
        let sans = vec!["localhost".into(), "127.0.0.1".into(), "mybox.lan".into()];
        assert_eq!(
            pick_url(Some("https://localhost:3345"), None, true, 3345, &sans),
            Ok("https://mybox.lan:3345".into())
        );
        assert_eq!(
            pick_url(Some("http://127.0.0.1:3344"), None, true, 3345, &sans[..2]),
            Err("localhost")
        );
    }
}
