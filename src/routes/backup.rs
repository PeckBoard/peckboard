use axum::{
    Router,
    extract::State,
    http::{StatusCode, header},
    middleware,
    response::IntoResponse,
    routing::get,
};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::auth::middleware::{require_admin, require_auth};
use crate::service::backup::{BackupSettings, create_snapshot};
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/admin/backup", get(download_backup))
        .route("/api/admin/backup/status", get(backup_status))
        .route_layer(middleware::from_fn(require_admin))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

async fn download_backup(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match create_snapshot(&state.db, &state.config.data_dir).await {
        Ok(bytes) => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let disposition = format!("attachment; filename=\"peckboard-backup-{now}.tar.gz\"");
            (
                StatusCode::OK,
                [
                    (
                        header::CONTENT_TYPE.as_str(),
                        "application/gzip".to_string(),
                    ),
                    (header::CONTENT_DISPOSITION.as_str(), disposition),
                ],
                bytes,
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("backup snapshot failed: {e:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("backup failed: {e}"),
            )
                .into_response()
        }
    }
}

async fn backup_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let settings = BackupSettings::load(&state.config.data_dir);
    axum::Json(serde_json::json!({
        "scheduled": settings.is_some(),
        "intervalHours": settings.as_ref().map(|s| s.interval_hours),
        "dir": settings.as_ref().map(|s| s.dir.display().to_string()),
        "retention": settings.as_ref().map(|s| s.retention),
    }))
}
