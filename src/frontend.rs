use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "web/dist/"]
struct Assets;

/// Serve an embedded static file, or fall back to index.html for SPA routing.
pub async fn static_handler(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');

    // Try the exact path first.
    if let Some(file) = Assets::get(path) {
        return file_response(path, &file.data);
    }

    // SPA fallback: serve index.html for any unmatched route.
    match Assets::get("index.html") {
        Some(file) => file_response("index.html", &file.data),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

fn file_response(path: &str, data: &[u8]) -> Response {
    let mime = mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string();

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, mime)],
        data.to_vec(),
    )
        .into_response()
}
