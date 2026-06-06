pub mod auth;
pub mod git;
pub mod misc;
pub mod projects;
pub mod reports;
pub mod sessions;

use crate::frontend::static_handler;
use crate::state::AppState;
use axum::{Router, routing::get};
use std::sync::Arc;

pub fn api_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/health", get(health))
        // Future: .merge(sessions::router())  — /api/sessions/*
        //         .merge(projects::router())  — /api/projects/*
        //         .merge(reports::router())   — /api/reports/*
        //         .merge(git::router())       — /api/git/*
        //         .merge(auth::router())      — /api/auth/*
        .fallback(static_handler)
}

async fn health() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({ "ok": true }))
}
