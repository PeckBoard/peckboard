//! `agent_sandbox` setting: mode + extra writable paths, persisted in the
//! core settings store and mirrored into the process-wide sandbox config.
//!
//! `GET /api/settings/agent-sandbox` (any user: drives the "agents are
//! unsandboxed" banner) → `{mode, extra_rw, status}`.
//! `PUT /api/settings/agent-sandbox/config` (admin) `{mode, extra_rw?}` → the same.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;

use super::Mode;
use crate::db::Db;
use crate::routes::settings::{SETTINGS_COLLECTION, SETTINGS_NS};
use crate::state::AppState;

const KEY: &str = "agent_sandbox";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SandboxSetting {
    pub mode: Mode,
    #[serde(default)]
    pub extra_rw: Vec<String>,
}

impl Default for SandboxSetting {
    fn default() -> Self {
        Self {
            mode: Mode::platform_default(),
            extra_rw: Vec::new(),
        }
    }
}

impl SandboxSetting {
    pub fn extra_paths(&self) -> Vec<PathBuf> {
        self.extra_rw.iter().map(PathBuf::from).collect()
    }
}

/// Read the stored setting (default when unset or unreadable). Blocking.
pub fn load_blocking(db: &Db) -> SandboxSetting {
    match db.plugin_store_get_blocking(SETTINGS_NS, SETTINGS_COLLECTION, KEY) {
        Ok(Some(json)) => serde_json::from_str(&json).unwrap_or_default(),
        _ => SandboxSetting::default(),
    }
}

fn validate(s: &SandboxSetting) -> Result<(), String> {
    for p in &s.extra_rw {
        let path = std::path::Path::new(p);
        if !path.is_absolute() {
            return Err(format!("'{p}' must be an absolute path"));
        }
        if path.parent().is_none() {
            return Err("the filesystem root cannot be made writable".into());
        }
    }
    Ok(())
}

fn body(setting: &SandboxSetting) -> serde_json::Value {
    serde_json::json!({
        "mode": setting.mode,
        "extra_rw": setting.extra_rw,
        "status": super::status(),
    })
}

pub async fn get_sandbox(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let db = state.db.clone();
    let setting = tokio::task::spawn_blocking(move || load_blocking(&db))
        .await
        .unwrap_or_default();
    Json(body(&setting))
}

pub async fn put_sandbox(
    State(state): State<Arc<AppState>>,
    Json(setting): Json<SandboxSetting>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let bad = |e: String| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e })),
        )
    };
    validate(&setting).map_err(bad)?;
    let db = state.db.clone();
    let value = serde_json::to_string(&setting).unwrap_or_default();
    let res = tokio::task::spawn_blocking(move || {
        db.plugin_store_put_blocking(SETTINGS_NS, SETTINGS_COLLECTION, KEY, &value)
    })
    .await;
    match res {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            ));
        }
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            ));
        }
    }
    super::update(setting.mode, setting.extra_paths());
    tracing::info!(
        mode = setting.mode.as_str(),
        "agent sandbox setting changed"
    );
    Ok(Json(body(&setting)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_relative_and_root_paths() {
        let mk = |p: &str| SandboxSetting {
            mode: Mode::Enforce,
            extra_rw: vec![p.into()],
        };
        assert!(validate(&mk("relative/dir")).is_err());
        assert!(validate(&mk("/")).is_err());
        assert!(validate(&mk("/srv/repos")).is_ok());
    }

    #[test]
    fn stored_value_round_trips_and_defaults() {
        let db = Db::in_memory().unwrap();
        assert_eq!(load_blocking(&db).mode, Mode::platform_default());
        let s = SandboxSetting {
            mode: Mode::Warn,
            extra_rw: vec!["/srv/x".into()],
        };
        db.plugin_store_put_blocking(
            SETTINGS_NS,
            SETTINGS_COLLECTION,
            KEY,
            &serde_json::to_string(&s).unwrap(),
        )
        .unwrap();
        let back = load_blocking(&db);
        assert_eq!(back.mode, Mode::Warn);
        assert_eq!(back.extra_rw, vec!["/srv/x".to_string()]);
    }
}
