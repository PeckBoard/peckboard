use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::get,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::auth::middleware::require_auth;
use crate::state::AppState;

#[derive(Deserialize)]
struct GitQuery {
    path: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    50
}

#[derive(Deserialize)]
struct DiffQuery {
    path: String,
}

#[derive(Serialize)]
struct DiscoveredRepo {
    name: String,
    path: String,
    folder_id: String,
    folder_name: String,
}

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/git/repos", get(scan_repos))
        .route("/api/git/status", get(git_status))
        .route("/api/git/diff", get(git_diff))
        .route("/api/git/commits", get(git_commits))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// GET /api/git/status?path=<path> — git status for a folder
async fn git_status(Query(q): Query<DiffQuery>) -> impl IntoResponse {
    let output = tokio::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&q.path)
        .output()
        .await;

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let files: Vec<&str> = stdout.lines().collect();
            Ok(Json(serde_json::json!({
                "path": q.path,
                "files": files,
                "clean": files.is_empty(),
            })))
        }
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("git status failed: {e}")})),
        )),
    }
}

/// GET /api/git/diff?path=<path> — git diff for a folder
async fn git_diff(Query(q): Query<DiffQuery>) -> impl IntoResponse {
    let output = tokio::process::Command::new("git")
        .args(["diff"])
        .current_dir(&q.path)
        .output()
        .await;

    match output {
        Ok(out) => {
            let diff = String::from_utf8_lossy(&out.stdout).to_string();
            Ok(Json(serde_json::json!({
                "path": q.path,
                "diff": diff,
            })))
        }
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("git diff failed: {e}")})),
        )),
    }
}

/// GET /api/git/commits?path=<path>&limit=<n> — recent commits
async fn git_commits(Query(q): Query<GitQuery>) -> impl IntoResponse {
    let limit_str = q.limit.to_string();
    let output = tokio::process::Command::new("git")
        .args([
            "log",
            &format!("-{limit_str}"),
            "--pretty=format:%H|%an|%ae|%at|%s",
        ])
        .current_dir(&q.path)
        .output()
        .await;

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let commits: Vec<serde_json::Value> = stdout
                .lines()
                .filter_map(|line| {
                    let parts: Vec<&str> = line.splitn(5, '|').collect();
                    if parts.len() == 5 {
                        Some(serde_json::json!({
                            "hash": parts[0],
                            "author": parts[1],
                            "email": parts[2],
                            "timestamp": parts[3].parse::<i64>().unwrap_or(0),
                            "message": parts[4],
                        }))
                    } else {
                        None
                    }
                })
                .collect();

            Ok(Json(serde_json::json!({
                "path": q.path,
                "commits": commits,
            })))
        }
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("git log failed: {e}")})),
        )),
    }
}

/// GET /api/git/repos — scan all folder paths from the database for git repos
async fn scan_repos(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let folders = state.db.list_folders().await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("failed to list folders: {e}")})),
        )
    })?;

    let mut repos: Vec<DiscoveredRepo> = Vec::new();

    for folder in &folders {
        let folder_path = std::path::Path::new(&folder.path);

        // Check if the folder itself is a git repo
        if folder_path.join(".git").exists() {
            let name = folder_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&folder.name)
                .to_string();
            repos.push(DiscoveredRepo {
                name,
                path: folder.path.clone(),
                folder_id: folder.id.clone(),
                folder_name: folder.name.clone(),
            });
        }

        // Check immediate subdirectories for .git
        if let Ok(entries) = std::fs::read_dir(folder_path) {
            for entry in entries.flatten() {
                let entry_path = entry.path();
                if entry_path.is_dir() && entry_path.join(".git").exists() {
                    let name = entry_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown")
                        .to_string();
                    repos.push(DiscoveredRepo {
                        name,
                        path: entry_path.to_string_lossy().to_string(),
                        folder_id: folder.id.clone(),
                        folder_name: folder.name.clone(),
                    });
                }
            }
        }
    }

    Ok::<_, (StatusCode, Json<serde_json::Value>)>(Json(serde_json::json!(repos)))
}
