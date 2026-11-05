//! `GET /mockup-frame` — the page an agent's HTML mockup runs inside.
//!
//! The app's CSP is `script-src 'self'` and a `srcdoc` iframe inherits it, so
//! a mockup written straight into the app's own frame renders but never runs
//! its scripts. This page carries its own policy instead, and starts empty:
//! the app posts the mockup in once the frame reports ready, so nothing is
//! stored and no mockup content ever lands in a URL.
//!
//! What keeps a mockup away from the user's account is the `sandbox` directive
//! below, not the network rules. It gives the document an opaque origin, so it
//! cannot read the login token in `localStorage`, and its requests to this
//! server carry `Origin: null`, which `origin_check` refuses. It is stated in
//! the header as well as on the `<iframe>` so that opening this URL directly,
//! with no iframe attribute involved, is sandboxed too.
//!
//! The route is public because it holds nothing: an iframe navigation cannot
//! carry the bearer token, and the page only ever renders what its parent
//! frame posts to it.

use axum::{
    Router,
    http::{HeaderName, HeaderValue, header},
    response::IntoResponse,
    routing::get,
};
use std::sync::Arc;

use crate::state::AppState;

pub const MOCKUP_FRAME_PATH: &str = "/mockup-frame";

/// `https:` sources and `'unsafe-eval'` are what let a mockup use CDN libraries
/// and in-browser compilers (Tailwind's CDN build, Babel). Network access is
/// deliberate: mockups may show live data from public APIs.
const CSP: &str = "sandbox allow-scripts; \
     default-src 'none'; \
     script-src 'unsafe-inline' 'unsafe-eval' https: data: blob:; \
     style-src 'unsafe-inline' https: data:; \
     img-src https: data: blob:; \
     font-src https: data:; \
     media-src https: data: blob:; \
     connect-src https: wss:; \
     worker-src blob:; \
     object-src 'none'; \
     base-uri 'none'; \
     form-action 'none'; \
     frame-ancestors 'self'";

/// Waits for the one message from the parent frame and replaces itself with
/// it. `document.open` clears this listener along with the rest of the page,
/// so the mockup's own scripts see a clean window.
const BOOTSTRAP: &str = r#"<!doctype html>
<meta charset="utf-8">
<title>Mockup</title>
<script>
(function () {
  if (window.parent === window) return;
  function onMessage(e) {
    if (e.source !== window.parent || !e.data) return;
    if (e.data.type !== 'mockup-frame-load' || typeof e.data.html !== 'string') return;
    window.removeEventListener('message', onMessage);
    document.open();
    document.write(e.data.html);
    document.close();
  }
  window.addEventListener('message', onMessage);
  window.parent.postMessage({ type: 'mockup-frame-ready' }, '*');
})();
</script>
"#;

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route(MOCKUP_FRAME_PATH, get(mockup_frame))
}

async fn mockup_frame() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CONTENT_SECURITY_POLICY, CSP),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::CACHE_CONTROL, "no-cache"),
        ]
        .map(|(name, value): (HeaderName, &'static str)| (name, HeaderValue::from_static(value))),
        BOOTSTRAP,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::security_headers;
    use axum::{
        body::Body,
        http::{Request, Response},
        middleware,
    };
    use tower::ServiceExt;

    /// The frame route behind the real middleware, next to an ordinary route.
    async fn get_path(path: &str) -> Response<Body> {
        let app: Router = Router::new()
            .route(MOCKUP_FRAME_PATH, get(mockup_frame))
            .route("/other", get(|| async { "ok" }))
            .layer(middleware::from_fn(security_headers));
        app.oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn frame_keeps_its_own_policy_and_other_routes_keep_the_global_one() {
        let frame = get_path(MOCKUP_FRAME_PATH).await;
        let csp = frame.headers()["Content-Security-Policy"].to_str().unwrap();
        // The opaque origin is the boundary that protects the user's account.
        assert!(csp.starts_with("sandbox allow-scripts;"));
        assert!(!csp.contains("allow-same-origin"));
        assert!(csp.contains("frame-ancestors 'self'"));
        // Inline scripts are the reason this route exists at all.
        assert!(csp.contains("script-src 'unsafe-inline'"));
        // DENY would stop the app from framing its own page.
        assert!(frame.headers().get("X-Frame-Options").is_none());

        let other = get_path("/other").await;
        let csp = other.headers()["Content-Security-Policy"].to_str().unwrap();
        assert!(csp.contains("script-src 'self'"));
        assert!(!csp.contains("unsafe-inline"));
        assert_eq!(other.headers()["X-Frame-Options"], "DENY");
    }
}
