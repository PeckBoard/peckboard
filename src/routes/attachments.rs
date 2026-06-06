use axum::{
    Json, Router,
    extract::{Path, State},
    http::{StatusCode, header},
    middleware,
    response::IntoResponse,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::auth::middleware::require_auth;
use crate::state::AppState;

const MAX_UPLOAD_SIZE: usize = 10 * 1024 * 1024; // 10 MB

const ALLOWED_EXTENSIONS: &[&str] = &[
    "txt", "md", "rs", "py", "js", "ts", "json", "toml", "yaml", "yml",
    "html", "css", "csv", "log", "sh", "sql", "xml",
    "png", "jpg", "jpeg", "gif", "svg", "pdf", "zip",
];

#[derive(Deserialize)]
struct UploadRequest {
    filename: String,
    data: String, // base64-encoded
}

#[derive(Serialize)]
struct UploadResponse {
    id: String,
    filename: String,
    size: u64,
}

#[derive(Serialize)]
struct AttachmentInfo {
    id: String,
    filename: String,
    size: u64,
}

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/sessions/{id}/attachments",
            post(upload_attachment).get(list_attachments),
        )
        .route(
            "/api/sessions/{id}/attachments/{aid}",
            get(download_attachment).delete(delete_attachment),
        )
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// Extract extension from filename; returns lowercase.
fn get_extension(filename: &str) -> Option<String> {
    std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
}

/// POST /api/sessions/:id/attachments
async fn upload_attachment(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    Json(body): Json<UploadRequest>,
) -> impl IntoResponse {
    // Validate extension
    let ext = match get_extension(&body.filename) {
        Some(e) if ALLOWED_EXTENSIONS.contains(&e.as_str()) => e,
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "file extension not allowed" })),
            ));
        }
    };

    // Decode base64
    use base64::Engine as _;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&body.data)
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "invalid base64 data" })),
            )
        })?;

    // Check size
    if decoded.len() > MAX_UPLOAD_SIZE {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "file exceeds 10MB limit" })),
        ));
    }

    // Build storage path: <dataDir>/attachments/<session_id>/<uuid>.<ext>
    let attachment_id = uuid::Uuid::new_v4().to_string();
    let stored_name = format!("{}.{}", attachment_id, ext);
    let dir = state
        .config
        .data_dir
        .join("attachments")
        .join(&session_id);

    tokio::fs::create_dir_all(&dir).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    let file_path = dir.join(&stored_name);
    tokio::fs::write(&file_path, &decoded).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    // Store the original filename in a sidecar metadata file
    let meta_path = dir.join(format!("{}.meta", attachment_id));
    tokio::fs::write(&meta_path, &body.filename)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>((
        StatusCode::CREATED,
        Json(serde_json::json!(UploadResponse {
            id: attachment_id,
            filename: body.filename,
            size: decoded.len() as u64,
        })),
    ))
}

/// GET /api/sessions/:id/attachments
async fn list_attachments(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    let dir = state
        .config
        .data_dir
        .join("attachments")
        .join(&session_id);

    let mut attachments: Vec<AttachmentInfo> = Vec::new();

    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(e) => e,
        Err(_) => {
            // Directory doesn't exist = no attachments
            return Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(
                serde_json::json!(attachments),
            ));
        }
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        // Skip .meta sidecar files
        if name.ends_with(".meta") {
            continue;
        }
        // Extract id (everything before the last dot)
        let id = match name.rfind('.') {
            Some(pos) => name[..pos].to_string(),
            None => continue,
        };

        // Read original filename from meta sidecar
        let meta_path = dir.join(format!("{}.meta", id));
        let original_filename = tokio::fs::read_to_string(&meta_path)
            .await
            .unwrap_or_else(|_| name.clone());

        let size = entry
            .metadata()
            .await
            .map(|m| m.len())
            .unwrap_or(0);

        attachments.push(AttachmentInfo {
            id,
            filename: original_filename,
            size,
        });
    }

    Ok(Json(serde_json::json!(attachments)))
}

/// GET /api/sessions/:id/attachments/:aid
async fn download_attachment(
    State(state): State<Arc<AppState>>,
    Path((session_id, aid)): Path<(String, String)>,
) -> impl IntoResponse {
    let dir = state
        .config
        .data_dir
        .join("attachments")
        .join(&session_id);

    // Find the file matching this attachment id
    let file_path = find_attachment_file(&dir, &aid).await;
    let file_path = match file_path {
        Some(p) => p,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "attachment not found" })),
            ));
        }
    };

    // Read original filename from meta sidecar
    let meta_path = dir.join(format!("{}.meta", aid));
    let original_filename = tokio::fs::read_to_string(&meta_path)
        .await
        .unwrap_or_else(|_| {
            file_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        });

    let data = tokio::fs::read(&file_path).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    // Guess content type from extension
    let content_type = mime_guess::from_path(&file_path)
        .first_or_octet_stream()
        .to_string();

    let disposition = format!(
        "attachment; filename=\"{}\"",
        original_filename.replace('"', "\\\"")
    );

    Ok::<_, (StatusCode, Json<serde_json::Value>)>((
        [
            (header::CONTENT_TYPE, content_type),
            (header::CONTENT_DISPOSITION, disposition),
            (
                header::HeaderName::from_static("x-content-type-options"),
                "nosniff".to_string(),
            ),
        ],
        data,
    ))
}

/// DELETE /api/sessions/:id/attachments/:aid
async fn delete_attachment(
    State(state): State<Arc<AppState>>,
    Path((session_id, aid)): Path<(String, String)>,
) -> impl IntoResponse {
    let dir = state
        .config
        .data_dir
        .join("attachments")
        .join(&session_id);

    let file_path = find_attachment_file(&dir, &aid).await;
    let file_path = match file_path {
        Some(p) => p,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "attachment not found" })),
            ));
        }
    };

    tokio::fs::remove_file(&file_path).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    // Also remove the meta sidecar if it exists
    let meta_path = dir.join(format!("{}.meta", aid));
    let _ = tokio::fs::remove_file(&meta_path).await;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(StatusCode::NO_CONTENT)
}

/// Find the attachment file by id prefix (id.ext pattern) in the directory.
async fn find_attachment_file(
    dir: &std::path::Path,
    aid: &str,
) -> Option<std::path::PathBuf> {
    let mut entries = tokio::fs::read_dir(dir).await.ok()?;
    let prefix = format!("{}.", aid);

    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(&prefix) && !name.ends_with(".meta") {
            return Some(entry.path());
        }
    }

    None
}
