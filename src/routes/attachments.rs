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

use crate::auth::middleware::{require_admin, require_auth};
use crate::state::AppState;

const MAX_UPLOAD_SIZE: usize = 10 * 1024 * 1024; // 10 MB

#[derive(Deserialize)]
struct UploadRequest {
    #[serde(alias = "name")]
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
        // Layers run outer-to-inner on the request, so `require_admin`
        // is appended LAST and executes AFTER `require_auth` has put
        // `AuthUser` into the request extensions. Sessions don't carry
        // a `user_id` column, so we can't perform per-row ownership
        // checks without a migration; admin-gating keeps an
        // admin-created non-admin user from reading attachments by
        // guessing session UUIDs.
        .route_layer(middleware::from_fn(require_admin))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// Restrict path segments to a safe charset so a crafted URL can't escape
/// the attachments directory. UUIDs and the session IDs this app generates
/// both fit; anything else is a 400.
fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn bad_request(msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": msg })),
    )
}

fn internal(msg: String) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": msg })),
    )
}

#[cfg(unix)]
fn restrict_dir(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn restrict_dir(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn restrict_file(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_file(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

/// Strip control characters, quotes, and backslashes so the name can never
/// inject extra HTTP headers via Content-Disposition. Also collapse the
/// value to its leaf so a client can't smuggle path separators through.
fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control() && *c != '"' && *c != '\\')
        .collect();
    let leaf = std::path::Path::new(&cleaned)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let trimmed = leaf.trim();
    if trimmed.is_empty() {
        "attachment".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Build a Content-Disposition value with both a sanitized ASCII fallback
/// (`filename=`) and an RFC 5987 UTF-8 form (`filename*=`) so non-ASCII
/// names survive round-tripping without enabling header injection.
fn content_disposition(filename: &str) -> String {
    let safe = sanitize_filename(filename);
    let ascii_fallback: String = safe
        .chars()
        .map(|c| if c.is_ascii() && c != '"' { c } else { '_' })
        .collect();
    let encoded = rfc5987_encode(&safe);
    format!("attachment; filename=\"{ascii_fallback}\"; filename*=UTF-8''{encoded}")
}

/// Percent-encode per RFC 5987 attr-char: alphanumerics plus a small set
/// of punctuation pass through; everything else is percent-encoded by byte.
fn rfc5987_encode(s: &str) -> String {
    const UNRESERVED: &[u8] = b"!#$&+-.^_`|~";
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || UNRESERVED.contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn session_dir(state: &AppState, session_id: &str) -> std::path::PathBuf {
    state.config.data_dir.join("attachments").join(session_id)
}

/// Resolve an attachment id to its on-disk path.
///
/// New uploads are stored as bare `<uuid>` (no extension). Older uploads
/// from a previous version of this code stored `<uuid>.<ext>`; fall back
/// to a prefix scan so those keep working without a migration.
async fn resolve_attachment_path(dir: &std::path::Path, aid: &str) -> Option<std::path::PathBuf> {
    let direct = dir.join(aid);
    if tokio::fs::try_exists(&direct).await.unwrap_or(false) {
        return Some(direct);
    }
    let prefix = format!("{}.", aid);
    let mut entries = tokio::fs::read_dir(dir).await.ok()?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(&prefix) && !name.ends_with(".meta") {
            return Some(entry.path());
        }
    }
    None
}

/// POST /api/sessions/:id/attachments
async fn upload_attachment(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    Json(body): Json<UploadRequest>,
) -> impl IntoResponse {
    tracing::info!(session_id = %session_id, filename = %body.filename, "Uploading attachment");
    if !is_safe_id(&session_id) {
        return Err(bad_request("invalid session id"));
    }

    let filename = sanitize_filename(&body.filename);

    use base64::Engine as _;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&body.data)
        .map_err(|_| bad_request("invalid base64 data"))?;

    if decoded.len() > MAX_UPLOAD_SIZE {
        return Err(bad_request("file exceeds 10MB limit"));
    }

    let attachment_id = uuid::Uuid::new_v4().to_string();
    let dir = session_dir(&state, &session_id);

    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| internal(e.to_string()))?;
    let _ = restrict_dir(&dir);

    // No extension on disk — the UUID is the only on-disk identifier. The
    // original filename lives in the .meta sidecar.
    let file_path = dir.join(&attachment_id);
    tokio::fs::write(&file_path, &decoded)
        .await
        .map_err(|e| internal(e.to_string()))?;
    let _ = restrict_file(&file_path);

    let meta_path = dir.join(format!("{}.meta", attachment_id));
    tokio::fs::write(&meta_path, &filename)
        .await
        .map_err(|e| internal(e.to_string()))?;
    let _ = restrict_file(&meta_path);

    Ok::<_, (StatusCode, Json<serde_json::Value>)>((
        StatusCode::CREATED,
        Json(serde_json::json!(UploadResponse {
            id: attachment_id,
            filename,
            size: decoded.len() as u64,
        })),
    ))
}

/// GET /api/sessions/:id/attachments
async fn list_attachments(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    tracing::info!(session_id = %session_id, "Listing attachments");
    if !is_safe_id(&session_id) {
        return Err(bad_request("invalid session id"));
    }

    let dir = session_dir(&state, &session_id);
    let mut attachments: Vec<AttachmentInfo> = Vec::new();

    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(e) => e,
        Err(_) => {
            return Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!(
                attachments
            )));
        }
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".meta") {
            continue;
        }
        // New layout: file name IS the uuid. Legacy layout: <uuid>.<ext>.
        // Strip a trailing extension to get the stem; skip anything that
        // doesn't look like one of our uuids.
        let id = match name.find('.') {
            Some(i) => name[..i].to_string(),
            None => name.clone(),
        };
        if !is_safe_id(&id) {
            continue;
        }
        let meta_path = dir.join(format!("{}.meta", id));
        let original_filename = tokio::fs::read_to_string(&meta_path)
            .await
            .map(|s| sanitize_filename(&s))
            .unwrap_or_else(|_| id.clone());

        let size = entry.metadata().await.map(|m| m.len()).unwrap_or(0);

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
    tracing::info!(session_id = %session_id, attachment_id = %aid, "Downloading attachment");
    if !is_safe_id(&session_id) || !is_safe_id(&aid) {
        return Err(bad_request("invalid id"));
    }

    let dir = session_dir(&state, &session_id);
    let file_path = match resolve_attachment_path(&dir, &aid).await {
        Some(p) => p,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "attachment not found" })),
            ));
        }
    };

    let meta_path = dir.join(format!("{}.meta", aid));
    let original_filename = tokio::fs::read_to_string(&meta_path)
        .await
        .map(|s| sanitize_filename(&s))
        .unwrap_or_else(|_| aid.clone());

    let data = tokio::fs::read(&file_path)
        .await
        .map_err(|e| internal(e.to_string()))?;

    // Any file type is allowed, so never trust the bytes — always force a
    // download with a generic type, nosniff, and a sandboxed CSP so a
    // misbehaving client that does render the response gets nothing.
    Ok::<_, (StatusCode, Json<serde_json::Value>)>((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (
                header::CONTENT_DISPOSITION,
                content_disposition(&original_filename),
            ),
            (
                header::HeaderName::from_static("x-content-type-options"),
                "nosniff".to_string(),
            ),
            (
                header::HeaderName::from_static("content-security-policy"),
                "default-src 'none'; sandbox".to_string(),
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
    tracing::info!(session_id = %session_id, attachment_id = %aid, "Deleting attachment");
    if !is_safe_id(&session_id) || !is_safe_id(&aid) {
        return Err(bad_request("invalid id"));
    }

    let dir = session_dir(&state, &session_id);
    let file_path = match resolve_attachment_path(&dir, &aid).await {
        Some(p) => p,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "attachment not found" })),
            ));
        }
    };

    tokio::fs::remove_file(&file_path)
        .await
        .map_err(|e| internal(e.to_string()))?;

    let meta_path = dir.join(format!("{}.meta", aid));
    let _ = tokio::fs::remove_file(&meta_path).await;

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_safe_id_accepts_uuid_and_friends() {
        assert!(is_safe_id("550e8400-e29b-41d4-a716-446655440000"));
        assert!(is_safe_id("session_42"));
        assert!(is_safe_id("abc-DEF_123"));
    }

    #[test]
    fn is_safe_id_rejects_traversal_and_separators() {
        assert!(!is_safe_id(""));
        assert!(!is_safe_id(".."));
        assert!(!is_safe_id("../etc/passwd"));
        assert!(!is_safe_id("a/b"));
        assert!(!is_safe_id("a\\b"));
        assert!(!is_safe_id("a.b"));
        assert!(!is_safe_id("a b"));
        assert!(!is_safe_id("a\0b"));
        assert!(!is_safe_id(&"a".repeat(200)));
    }

    #[test]
    fn sanitize_filename_strips_control_chars_and_quotes() {
        assert_eq!(sanitize_filename("hello.txt"), "hello.txt");
        assert_eq!(sanitize_filename("a\r\nb.txt"), "ab.txt");
        assert_eq!(sanitize_filename("evil\"name.txt"), "evilname.txt");
        assert_eq!(
            sanitize_filename("with\\backslash.txt"),
            "withbackslash.txt"
        );
        assert_eq!(sanitize_filename("null\0byte.txt"), "nullbyte.txt");
    }

    #[test]
    fn sanitize_filename_keeps_only_leaf() {
        assert_eq!(sanitize_filename("/etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("../../boot.ini"), "boot.ini");
        assert_eq!(sanitize_filename("./readme.md"), "readme.md");
    }

    #[test]
    fn sanitize_filename_falls_back_when_empty() {
        assert_eq!(sanitize_filename(""), "attachment");
        assert_eq!(sanitize_filename("   "), "attachment");
        assert_eq!(sanitize_filename("\r\n\t"), "attachment");
    }

    #[test]
    fn content_disposition_includes_both_forms() {
        let h = content_disposition("report.pdf");
        assert!(h.starts_with("attachment; filename=\"report.pdf\""));
        assert!(h.contains("filename*=UTF-8''report.pdf"));
    }

    #[test]
    fn content_disposition_blocks_header_injection() {
        // Even with CR/LF and a fake header in the input, the output must
        // remain a single header line — control chars are stripped before
        // the value is emitted, so a new header can't be smuggled in.
        let h = content_disposition("evil\r\nSet-Cookie: x=1\r\n.txt");
        assert!(!h.contains('\r'));
        assert!(!h.contains('\n'));
    }

    #[test]
    fn content_disposition_encodes_unicode_in_5987_form() {
        let h = content_disposition("café.pdf");
        // ASCII fallback drops non-ASCII to underscores.
        assert!(h.contains("filename=\"caf_.pdf\""));
        // UTF-8 form percent-encodes the same bytes.
        assert!(h.contains("filename*=UTF-8''"));
        assert!(h.contains("caf%C3%A9"));
    }

    #[test]
    fn rfc5987_encode_basic() {
        assert_eq!(rfc5987_encode("hello"), "hello");
        assert_eq!(rfc5987_encode("a b"), "a%20b");
        assert_eq!(rfc5987_encode("é"), "%C3%A9");
        assert_eq!(rfc5987_encode("a/b"), "a%2Fb");
    }

    #[tokio::test]
    async fn resolve_path_prefers_bare_uuid() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "abc123";
        std::fs::write(tmp.path().join(id), b"x").unwrap();
        let resolved = resolve_attachment_path(tmp.path(), id).await.unwrap();
        assert_eq!(resolved, tmp.path().join(id));
    }

    #[tokio::test]
    async fn resolve_path_falls_back_to_legacy_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "legacyid";
        std::fs::write(tmp.path().join(format!("{id}.txt")), b"x").unwrap();
        let resolved = resolve_attachment_path(tmp.path(), id).await.unwrap();
        assert_eq!(resolved, tmp.path().join(format!("{id}.txt")));
    }

    #[tokio::test]
    async fn resolve_path_ignores_meta_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "metaonly";
        std::fs::write(tmp.path().join(format!("{id}.meta")), b"x").unwrap();
        assert!(resolve_attachment_path(tmp.path(), id).await.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn restrict_file_sets_owner_only_perms() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("f");
        std::fs::write(&p, b"x").unwrap();
        restrict_file(&p).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn restrict_dir_sets_owner_only_perms() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        restrict_dir(tmp.path()).unwrap();
        let mode = std::fs::metadata(tmp.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
