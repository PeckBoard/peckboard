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
use std::sync::Arc;

use crate::auth::middleware::require_auth;
use crate::state::AppState;

#[derive(Serialize)]
struct ReportMeta {
    folder: String,
    file: String,
    title: String,
    date: String,
    session_id: Option<String>,
    project_name: Option<String>,
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
            get(get_report).put(update_report),
        )
        .route(
            "/api/reports/{folder}/{file}/download",
            get(download_report),
        )
        .route_layer(middleware::from_fn_with_state(state, require_auth))
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
                                if let Ok(content) = std::fs::read_to_string(&file_path) {
                                    let meta =
                                        parse_frontmatter(&content, &folder_name, &file_name);
                                    reports.push(meta);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Json(serde_json::json!({ "reports": reports }))
}

/// GET /api/reports/:folder/:file — read report with frontmatter
async fn get_report(
    State(state): State<Arc<AppState>>,
    Path((folder, file)): Path<(String, String)>,
) -> impl IntoResponse {
    let sanitized_folder = sanitize_name(&folder);
    let sanitized_file = sanitize_name(&file);
    let file_path = state
        .config
        .data_dir
        .join("reports")
        .join(&sanitized_folder)
        .join(&sanitized_file);

    match std::fs::read_to_string(&file_path) {
        Ok(content) => {
            let meta = parse_frontmatter(&content, &sanitized_folder, &sanitized_file);
            let body = strip_frontmatter(&content);
            Ok(Json(ReportFull { meta, body }))
        }
        Err(_) => Err(StatusCode::NOT_FOUND),
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

    let sanitized_folder = sanitize_name(&folder);
    let sanitized_file = sanitize_name(&file);
    let file_path = state
        .config
        .data_dir
        .join("reports")
        .join(&sanitized_folder)
        .join(&sanitized_file);

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

/// GET /api/reports/:folder/:file/download — raw markdown download
async fn download_report(
    State(state): State<Arc<AppState>>,
    Path((folder, file)): Path<(String, String)>,
) -> impl IntoResponse {
    let sanitized_folder = sanitize_name(&folder);
    let sanitized_file = sanitize_name(&file);
    let file_path = state
        .config
        .data_dir
        .join("reports")
        .join(&sanitized_folder)
        .join(&sanitized_file);

    match std::fs::read_to_string(&file_path) {
        Ok(content) => {
            let disposition = format!("attachment; filename=\"{sanitized_file}\"");
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

/// Sanitize a filename component — no path separators, dots for traversal.
fn sanitize_name(name: &str) -> String {
    name.replace(['/', '\\', '\0'], "")
        .replace("..", "")
        .trim_matches('.')
        .to_string()
}

/// Parse YAML-ish frontmatter from a markdown file.
fn parse_frontmatter(content: &str, folder: &str, file: &str) -> ReportMeta {
    let mut title = file.trim_end_matches(".md").to_string();
    let mut date = folder.to_string();
    let mut session_id = None;
    let mut project_name = None;

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
            }
        }
    }

    ReportMeta {
        folder: folder.to_string(),
        file: file.to_string(),
        title,
        date,
        session_id,
        project_name,
    }
}

fn extract_frontmatter(content: &str) -> Option<String> {
    let content = content.trim_start();
    if !content.starts_with("---") {
        return None;
    }
    let rest = &content[3..];
    let end = rest.find("\n---")?;
    Some(rest[..end].trim().to_string())
}

fn strip_frontmatter(content: &str) -> String {
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
