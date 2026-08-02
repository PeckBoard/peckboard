use axum::{
    Json,
    Router,
    extract::{Path, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::get,
    // put is used via .put() method on route
};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::sync::Arc;

use crate::auth::middleware::require_auth;
use crate::state::AppState;

/// How many bytes of a report file the list endpoint reads to parse
/// frontmatter + build a preview. Frontmatter lives at the top of the file
/// and is always tiny; this bounds the list endpoint's I/O to a small,
/// constant amount per file regardless of report body size.
const LIST_HEAD_BYTES: u64 = 8 * 1024;

/// How many characters of the (frontmatter-stripped) body to surface as a
/// list-view preview.
const PREVIEW_CHARS: usize = 240;

#[derive(Serialize)]
struct ReportMeta {
    folder: String,
    file: String,
    title: String,
    date: String,
    session_id: Option<String>,
    session_name: Option<String>,
    session_created_at: Option<String>,
    project_name: Option<String>,
    /// First ~240 chars of the body (whitespace-collapsed), for list views.
    /// Not necessarily the whole report if it's larger than
    /// [`LIST_HEAD_BYTES`] — use `GET /api/reports/:folder/:file` for the
    /// full content.
    preview: String,
}

#[derive(Serialize)]
struct ReportFull {
    #[serde(flatten)]
    meta: ReportMeta,
    body: String,
}

#[derive(Deserialize)]
struct UpdateReportBody {
    body: String,
}

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/reports", get(list_reports))
        .route(
            "/api/reports/{folder}/{file}",
            get(get_report).put(update_report).delete(delete_report),
        )
        .route(
            "/api/reports/{folder}/{file}/download",
            get(download_report),
        )
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// Read up to `max_bytes` of a file as a lossy UTF-8 string. Used by the
/// list endpoint so it doesn't pull whole report bodies into memory just to
/// parse a few frontmatter lines.
fn read_head(path: &std::path::Path, max_bytes: u64) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    file.take(max_bytes).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Collapse whitespace and truncate the (frontmatter-stripped) body to
/// [`PREVIEW_CHARS`] characters for list-view display.
fn make_preview(body: &str) -> String {
    let collapsed = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= PREVIEW_CHARS {
        collapsed
    } else {
        let truncated: String = collapsed.chars().take(PREVIEW_CHARS).collect();
        format!("{truncated}…")
    }
}

/// GET /api/reports — list all report metadata
async fn list_reports(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let reports_dir = state.config.data_dir.join("reports");
    let mut reports = Vec::new();

    if reports_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&reports_dir) {
            for entry in entries.flatten() {
                let folder_path = entry.path();
                if folder_path.is_dir() {
                    let folder_name = entry.file_name().to_string_lossy().to_string();
                    if let Ok(files) = std::fs::read_dir(&folder_path) {
                        for file_entry in files.flatten() {
                            let file_path = file_entry.path();
                            if file_path.extension().map_or(false, |e| e == "md") {
                                let file_name =
                                    file_entry.file_name().to_string_lossy().to_string();
                                if let Some(head) = read_head(&file_path, LIST_HEAD_BYTES) {
                                    let meta = parse_frontmatter(&head, &folder_name, &file_name);
                                    reports.push(meta);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    sort_reports(&mut reports);

    Json(serde_json::json!({ "reports": reports }))
}

/// GET /api/reports/:folder/:file — read report with frontmatter
async fn get_report(
    State(state): State<Arc<AppState>>,
    Path((folder, file)): Path<(String, String)>,
) -> impl IntoResponse {
    let (safe_folder, safe_file) = match (safe_segment(&folder, false), safe_segment(&file, true)) {
        (Some(f), Some(fi)) => (f, fi),
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid path"})),
            ));
        }
    };
    let file_path = state
        .config
        .data_dir
        .join("reports")
        .join(&safe_folder)
        .join(&safe_file);

    match std::fs::read_to_string(&file_path) {
        Ok(content) => {
            let meta = parse_frontmatter(&content, &safe_folder, &safe_file);
            let body = strip_frontmatter(&content);
            Ok(Json(ReportFull { meta, body }))
        }
        Err(_) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not found"})),
        )),
    }
}

/// PUT /api/reports/:folder/:file — update report body (1MB cap)
async fn update_report(
    State(state): State<Arc<AppState>>,
    Path((folder, file)): Path<(String, String)>,
    Json(body): Json<UpdateReportBody>,
) -> impl IntoResponse {
    if body.body.len() > 1_048_576 {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({"error": "report body exceeds 1MB limit"})),
        ));
    }

    let (safe_folder, safe_file) = match (safe_segment(&folder, false), safe_segment(&file, true)) {
        (Some(f), Some(fi)) => (f, fi),
        _ => return Err(bad_path()),
    };
    let file_path = state
        .config
        .data_dir
        .join("reports")
        .join(&safe_folder)
        .join(&safe_file);

    if !file_path.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "report not found"})),
        ));
    }

    // Read existing frontmatter, replace body
    let existing = std::fs::read_to_string(&file_path).unwrap_or_default();
    let frontmatter = extract_frontmatter(&existing);
    let new_content = if let Some(fm) = frontmatter {
        format!("---\n{fm}\n---\n\n{}", body.body)
    } else {
        body.body.clone()
    };

    std::fs::write(&file_path, new_content).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?;

    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /api/reports/:folder/:file → 204. Missing file → 404.
async fn delete_report(
    State(state): State<Arc<AppState>>,
    Path((folder, file)): Path<(String, String)>,
) -> impl IntoResponse {
    let (safe_folder, safe_file) = match (safe_segment(&folder, false), safe_segment(&file, true)) {
        (Some(f), Some(fi)) => (f, fi),
        _ => return Err(bad_path()),
    };
    let file_path = state
        .config
        .data_dir
        .join("reports")
        .join(&safe_folder)
        .join(&safe_file);

    if !file_path.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "report not found"})),
        ));
    }

    std::fs::remove_file(&file_path).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?;

    Ok(StatusCode::NO_CONTENT)
}

/// GET /api/reports/:folder/:file/download — raw markdown download
async fn download_report(
    State(state): State<Arc<AppState>>,
    Path((folder, file)): Path<(String, String)>,
) -> impl IntoResponse {
    let (safe_folder, safe_file) = match (safe_segment(&folder, false), safe_segment(&file, true)) {
        (Some(f), Some(fi)) => (f, fi),
        _ => return Err(StatusCode::BAD_REQUEST),
    };
    let file_path = state
        .config
        .data_dir
        .join("reports")
        .join(&safe_folder)
        .join(&safe_file);

    match std::fs::read_to_string(&file_path) {
        Ok(content) => {
            let disposition = format!("attachment; filename=\"{safe_file}\"");
            Ok((
                StatusCode::OK,
                [
                    (
                        axum::http::header::CONTENT_TYPE.as_str(),
                        "text/markdown".to_string(),
                    ),
                    (
                        axum::http::header::CONTENT_DISPOSITION.as_str(),
                        disposition,
                    ),
                ],
                content,
            ))
        }
        Err(_) => Err(StatusCode::NOT_FOUND),
    }
}

/// Strict filename-component validator. Allows the same charset
/// `is_safe_id` in attachments uses (ASCII alphanumeric, `-`, `_`),
/// plus a single optional trailing `.md` suffix on the `file` segment.
/// Real report folders are `%Y-%m-%d` dates and files are sanitized to
/// this charset at write time (MCP report handlers), so nothing valid
/// is rejected. Anything else returns `None` so the caller can 400 the
/// request outright rather than silently scrubbing characters and
/// acting on the result.
///
/// The earlier `replace`-based scrubber accepted any input and
/// collapsed problematic substrings to nothing; that left it
/// vulnerable to inputs like `"."` (trims to `""`) and other edge
/// cases that the attachments module already addresses via its
/// stricter `is_safe_id`.
pub(crate) fn safe_segment(name: &str, allow_md: bool) -> Option<String> {
    if name.is_empty() || name.len() > 128 {
        return None;
    }
    // Strip a `.md` suffix (only) before validating.
    let core = if allow_md {
        name.strip_suffix(".md").unwrap_or(name)
    } else {
        name
    };
    if core.is_empty() {
        return None;
    }
    let ok = core
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok { Some(name.to_string()) } else { None }
}

fn bad_path() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error": "invalid path"})),
    )
}

/// Parse YAML-ish frontmatter from a markdown file.
fn parse_frontmatter(content: &str, folder: &str, file: &str) -> ReportMeta {
    let mut title = file.trim_end_matches(".md").to_string();
    let mut date = folder.to_string();
    let mut session_id = None;
    let mut project_name = None;
    let mut session_name = None;
    let mut session_created_at = None;

    if let Some(fm) = extract_frontmatter(content) {
        for line in fm.lines() {
            if let Some(val) = line.strip_prefix("title:") {
                title = val.trim().trim_matches('"').to_string();
            } else if let Some(val) = line.strip_prefix("date:") {
                date = val.trim().trim_matches('"').to_string();
            } else if let Some(val) = line.strip_prefix("sessionId:") {
                session_id = Some(val.trim().trim_matches('"').to_string());
            } else if let Some(val) = line.strip_prefix("projectName:") {
                project_name = Some(val.trim().trim_matches('"').to_string());
            } else if let Some(val) = line.strip_prefix("sessionName:") {
                session_name = Some(val.trim().trim_matches('"').to_string());
            } else if let Some(val) = line.strip_prefix("sessionCreatedAt:") {
                session_created_at = Some(val.trim().trim_matches('"').to_string());
            }
        }
    }

    let body_preview = make_preview(&strip_frontmatter(content));

    ReportMeta {
        folder: folder.to_string(),
        file: file.to_string(),
        title,
        date,
        session_id,
        project_name,
        session_name,
        session_created_at,
        preview: body_preview,
    }
}

/// Public helper: extract just the report's display title from its
/// markdown content. Falls back to the file stem when the frontmatter
/// is missing or has no `title:` line. Used by the tabs endpoint
/// (`src/routes/me.rs`) to label report tabs without duplicating the
/// frontmatter parser.
pub(crate) fn report_title(content: &str, file: &str) -> String {
    if let Some(fm) = extract_frontmatter(content) {
        for line in fm.lines() {
            if let Some(val) = line.strip_prefix("title:") {
                let t = val.trim().trim_matches('"').trim().to_string();
                if !t.is_empty() {
                    return t;
                }
            }
        }
    }
    file.trim_end_matches(".md").to_string()
}

pub(crate) fn extract_frontmatter(content: &str) -> Option<String> {
    let content = content.trim_start();
    if !content.starts_with("---") {
        return None;
    }
    let rest = &content[3..];
    let end = rest.find("\n---")?;
    Some(rest[..end].trim().to_string())
}

pub(crate) fn strip_frontmatter(content: &str) -> String {
    let content = content.trim_start();
    if !content.starts_with("---") {
        return content.to_string();
    }
    let rest = &content[3..];
    match rest.find("\n---") {
        Some(end) => rest[end + 4..].trim_start().to_string(),
        None => content.to_string(),
    }
}

/// Sort reports newest-first by their frontmatter `date` (RFC3339).
/// Unparseable dates fall back to a lexical (folder, file) comparison and
/// sort after every report whose date parsed.
fn sort_reports(reports: &mut [ReportMeta]) {
    reports.sort_by(|a, b| {
        let da = chrono::DateTime::parse_from_rfc3339(&a.date).ok();
        let db = chrono::DateTime::parse_from_rfc3339(&b.date).ok();
        match (da, db) {
            (Some(x), Some(y)) => y.cmp(&x),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => {
                (b.folder.as_str(), b.file.as_str()).cmp(&(a.folder.as_str(), a.file.as_str()))
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(folder: &str, file: &str, date: &str) -> ReportMeta {
        ReportMeta {
            folder: folder.to_string(),
            file: file.to_string(),
            title: String::new(),
            date: date.to_string(),
            session_id: None,
            session_name: None,
            session_created_at: None,
            project_name: None,
            preview: String::new(),
        }
    }

    #[test]
    fn make_preview_collapses_whitespace_and_truncates() {
        let long = "word ".repeat(100);
        let preview = make_preview(&long);
        assert!(preview.chars().count() <= PREVIEW_CHARS + 1);
        assert!(preview.ends_with('…'));
        assert!(!preview.contains("  "));
    }

    #[test]
    fn make_preview_keeps_short_body_untouched() {
        assert_eq!(make_preview("hello   world\n\nfoo"), "hello world foo");
    }

    #[test]
    fn sorts_newest_first_unparseable_last() {
        let mut v = vec![
            meta("2026-07-01", "a.md", "2026-07-01T10:00:00+00:00"),
            meta("2026-07-09", "b.md", "2026-07-09T08:00:00+00:00"),
            meta("zzz", "bad.md", "not-a-date"),
            meta("2026-07-05", "c.md", "2026-07-05T23:00:00+00:00"),
        ];
        sort_reports(&mut v);
        let order: Vec<&str> = v.iter().map(|m| m.file.as_str()).collect();
        assert_eq!(order, vec!["b.md", "c.md", "a.md", "bad.md"]);
    }

    #[test]
    fn respects_timezone_offsets() {
        // 12:00+02:00 == 10:00Z is earlier than 11:00Z.
        let mut v = vec![
            meta("d", "tz.md", "2026-07-09T12:00:00+02:00"),
            meta("d", "utc.md", "2026-07-09T11:00:00+00:00"),
        ];
        sort_reports(&mut v);
        let order: Vec<&str> = v.iter().map(|m| m.file.as_str()).collect();
        assert_eq!(order, vec!["utc.md", "tz.md"]);
    }
}
