//! Serves files agents shared into the chat (`service::shared_files`).
//!
//! - `POST /api/sessions/{id}/shared-files/{fid}/url` — JWT + per-session
//!   access; mints a 10-minute signed URL for one snapshot.
//! - `GET /api/shared-files/{fid}?sid&exp&sig[&download=1]` — NO JWT: the
//!   HMAC signature over `(sid, fid, exp)` is the credential, so `<img>`,
//!   `<video>` and plain download links work. Streams the file with
//!   single-range support (`<video>` seeking).

use crate::auth::middleware::{require_auth, require_session_access};
use crate::service::shared_files;
use crate::state::AppState;
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let authed = Router::new()
        .route("/api/sessions/{id}/shared-files/{fid}/url", post(mint_url))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_session_access,
        ))
        .route_layer(middleware::from_fn_with_state(state, require_auth));
    Router::new()
        .route("/api/shared-files/{fid}", get(serve))
        .merge(authed)
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}

/// POST /api/sessions/:id/shared-files/:fid/url
async fn mint_url(
    State(state): State<Arc<AppState>>,
    Path((session_id, file_id)): Path<(String, String)>,
) -> Response {
    if shared_files::lookup(&state.config.data_dir, &session_id, &file_id)
        .await
        .is_none()
    {
        return err(StatusCode::NOT_FOUND, "shared file not found");
    }
    let url = shared_files::signed_url(
        &state.jwt_secret,
        &session_id,
        &file_id,
        shared_files::now_secs(),
    );
    Json(serde_json::json!({ "url": url })).into_response()
}

#[derive(Deserialize)]
struct ServeQuery {
    sid: String,
    exp: u64,
    sig: String,
    #[serde(default)]
    download: Option<String>,
}

/// Types the browser may render inline. Everything else — notably SVG and
/// HTML, which can carry script — is served as an opaque attachment.
fn inline_content_type(mime: &str) -> Option<&'static str> {
    Some(match mime {
        "image/png" => "image/png",
        "image/jpeg" => "image/jpeg",
        "image/gif" => "image/gif",
        "image/webp" => "image/webp",
        "video/mp4" => "video/mp4",
        "video/webm" => "video/webm",
        "video/quicktime" => "video/quicktime",
        "video/x-m4v" => "video/x-m4v",
        "video/ogg" => "video/ogg",
        "audio/mpeg" => "audio/mpeg",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "audio/wav",
        "audio/ogg" | "audio/opus" => "audio/ogg",
        "audio/flac" | "audio/x-flac" => "audio/flac",
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" => "audio/mp4",
        "audio/aac" | "audio/x-aac" => "audio/aac",
        "audio/webm" => "audio/webm",
        "application/pdf" => "application/pdf",
        "application/json" => "text/plain; charset=utf-8",
        m if m.starts_with("text/") && m != "text/html" => "text/plain; charset=utf-8",
        _ => return None,
    })
}

/// GET /api/shared-files/:fid
async fn serve(
    State(state): State<Arc<AppState>>,
    Path(file_id): Path<String>,
    Query(q): Query<ServeQuery>,
    headers: HeaderMap,
) -> Response {
    if !shared_files::is_safe_id(&q.sid) || !shared_files::is_safe_id(&file_id) {
        return err(StatusCode::BAD_REQUEST, "invalid id");
    }
    if !shared_files::verify(
        &state.jwt_secret,
        &q.sid,
        &file_id,
        q.exp,
        &q.sig,
        shared_files::now_secs(),
    ) {
        return err(StatusCode::FORBIDDEN, "invalid or expired link");
    }
    let Some((meta, blob)) = shared_files::lookup(&state.config.data_dir, &q.sid, &file_id).await
    else {
        return err(StatusCode::NOT_FOUND, "shared file not found");
    };
    let mut file = match tokio::fs::File::open(&blob).await {
        Ok(f) => f,
        Err(_) => return err(StatusCode::NOT_FOUND, "shared file not found"),
    };
    let size = match file.metadata().await {
        Ok(m) => m.len(),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let inline = inline_content_type(&meta.mime_type);
    let want_download = q.download.as_deref().is_some_and(|v| v != "0");
    let content_type = inline.unwrap_or("application/octet-stream");

    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let (start, end, status) = match shared_files::parse_range(range, size) {
        Ok(Some((s, e))) => (s, e, StatusCode::PARTIAL_CONTENT),
        Ok(None) => (0, size.saturating_sub(1), StatusCode::OK),
        Err(()) => {
            let mut r = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            if let Ok(v) = HeaderValue::from_str(&format!("bytes */{size}")) {
                r.headers_mut().insert(header::CONTENT_RANGE, v);
            }
            return r;
        }
    };
    let len = if size == 0 { 0 } else { end - start + 1 };
    if start > 0
        && let Err(e) = file.seek(std::io::SeekFrom::Start(start)).await
    {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }

    // Stream in 64 KiB chunks — a 2 GB build artifact never sits in memory.
    // `fuse`: hyper may poll once more after the end, which a bare `unfold`
    // treats as a bug and panics on. A read error ends the stream.
    let stream = futures_util::stream::unfold(Some(file.take(len)), |r| async move {
        let mut r = r?;
        let mut buf = vec![0u8; 64 * 1024];
        match r.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok(axum::body::Bytes::from(buf)), Some(r)))
            }
            Err(e) => Some((Err::<axum::body::Bytes, _>(e), None)),
        }
    });
    let stream = futures_util::StreamExt::fuse(stream);

    let mut resp = Response::new(Body::from_stream(stream));
    *resp.status_mut() = status;
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if status == StatusCode::PARTIAL_CONTENT
        && let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{end}/{size}"))
    {
        h.insert(header::CONTENT_RANGE, v);
    }
    if (want_download || inline.is_none())
        && let Ok(v) = HeaderValue::from_str(&crate::routes::attachments::content_disposition(
            &meta.filename,
        ))
    {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=600"),
    );
    h.insert(
        header::HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static("default-src 'none'; sandbox"),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scriptable_types_are_never_inline() {
        assert_eq!(inline_content_type("image/png"), Some("image/png"));
        assert_eq!(inline_content_type("video/mp4"), Some("video/mp4"));
        assert_eq!(
            inline_content_type("text/markdown"),
            Some("text/plain; charset=utf-8")
        );
        assert_eq!(inline_content_type("image/svg+xml"), None);
        assert_eq!(inline_content_type("text/html"), None);
        assert_eq!(inline_content_type("application/zip"), None);
    }

    #[test]
    fn playable_media_extensions_serve_inline() {
        for name in [
            "a.mp4", "a.webm", "a.mov", "a.m4v", "a.ogv", "a.mp3", "a.wav", "a.ogg", "a.oga",
            "a.opus", "a.flac", "a.m4a", "a.aac", "a.weba",
        ] {
            let mime = shared_files::guess_mime(name);
            assert!(
                inline_content_type(&mime).is_some(),
                "{name} ({mime}) must play inline"
            );
        }
    }
}
