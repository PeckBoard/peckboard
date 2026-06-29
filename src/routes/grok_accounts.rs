//! `/api/grok-accounts` — manage the set of Grok / xAI credentials the
//! spawned `grok` CLI can run as. Mirrors [`super::claude_accounts`]; see that
//! module for the budget/usage model. The "Default" account (host `~/.grok`)
//! is implicit and never appears here; a session uses an account by carrying
//! `@<account_id>` on its model id.
//!
//! Grok's login is a browser device-code flow run by the CLI itself, so —
//! unlike Claude's paste-back exchange — an account is created first (in a
//! not-yet-authenticated state) and then signed in via
//! `POST /api/grok-accounts/{id}/login/start`, which returns a URL to open.
//! The account reads as authenticated once `grok login` writes its
//! `auth.json` (see [`crate::provider::grok::login`]).

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::get,
};
use serde::{Deserialize, Serialize};

use crate::auth::middleware::require_auth;
use crate::db::models::{GrokAccount, GrokAccountChanges, NewGrokAccount};
use crate::provider::grok::login::{self, GROK_LOGIN};
use crate::routes::usage::cost::usage_cost;
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/grok-accounts",
            get(list_accounts).post(create_account),
        )
        .route(
            "/api/grok-accounts/{id}",
            axum::routing::put(update_account).delete(delete_account),
        )
        .route(
            "/api/grok-accounts/{id}/login/start",
            axum::routing::post(start_login),
        )
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

type ApiError = (StatusCode, Json<serde_json::Value>);

fn bad_request(msg: &str) -> ApiError {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": msg })),
    )
}

fn server_error(msg: impl std::fmt::Display) -> ApiError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": msg.to_string() })),
    )
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ── Wire shapes ──────────────────────────────────────────────────────

/// Mirrors the Claude account warn levels so the UI can reuse the same badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum WarnLevel {
    None,
    Ok,
    Warning,
    Critical,
    Exceeded,
}

#[derive(Debug, Clone, Serialize)]
struct AccountUsage {
    total_tokens: i64,
    est_cost_usd: f64,
    turns: i64,
    used_fraction: Option<f64>,
    level: WarnLevel,
}

/// One account as returned to the UI. The `credential` is never sent back;
/// `authenticated` tells the UI whether a device account has finished its
/// browser sign-in (or an api_key account has a key set).
#[derive(Debug, Clone, Serialize)]
struct AccountView {
    id: String,
    name: String,
    kind: String,
    authenticated: bool,
    config_dir: Option<String>,
    budget_window_hours: Option<i32>,
    budget_limit_usd: Option<f64>,
    budget_limit_tokens: Option<i64>,
    warn_threshold: f64,
    critical_threshold: f64,
    created_at: i64,
    updated_at: i64,
    usage: AccountUsage,
}

#[derive(Debug, Deserialize)]
struct CreateAccountBody {
    name: String,
    /// `"device"` (browser sign-in, default) or `"api_key"`.
    #[serde(default)]
    kind: String,
    /// Required for an `api_key` account; ignored for `device`.
    #[serde(default)]
    credential: Option<String>,
    #[serde(default)]
    budget_window_hours: Option<i32>,
    #[serde(default)]
    budget_limit_usd: Option<f64>,
    #[serde(default)]
    budget_limit_tokens: Option<i64>,
    #[serde(default)]
    warn_threshold: Option<f64>,
    #[serde(default)]
    critical_threshold: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct UpdateAccountBody {
    name: String,
    /// For an `api_key` account, a non-empty value replaces the stored key.
    /// Empty / absent leaves it untouched. Ignored for `device` accounts.
    #[serde(default)]
    credential: Option<String>,
    #[serde(default)]
    budget_window_hours: Option<i32>,
    #[serde(default)]
    budget_limit_usd: Option<f64>,
    #[serde(default)]
    budget_limit_tokens: Option<i64>,
    #[serde(default)]
    warn_threshold: Option<f64>,
    #[serde(default)]
    critical_threshold: Option<f64>,
}

// ── Helpers ──────────────────────────────────────────────────────────

/// The non-secret marker stored as a device account's `credential`; the real
/// credentials live in `config_dir/auth.json`.
const DEVICE_MARKER: &str = "device";

fn valid_kind(kind: &str) -> bool {
    matches!(kind, "device" | "api_key")
}

/// Whether the account is usable: a device account once `grok login` has
/// written its auth.json, an api_key account once it has a key.
fn is_authenticated(acct: &GrokAccount) -> bool {
    if acct.kind == "api_key" {
        !acct.credential.is_empty()
    } else {
        login::device_authenticated(acct.config_dir.as_deref())
    }
}

/// Thresholds must be ordered fractions in `(0, 1]`. Defaults (0.75 / 0.90).
fn normalize_thresholds(warn: Option<f64>, critical: Option<f64>) -> Result<(f64, f64), ApiError> {
    let warn = warn.unwrap_or(0.75);
    let critical = critical.unwrap_or(0.90);
    let in_range = |v: f64| v > 0.0 && v <= 1.0;
    if !in_range(warn) || !in_range(critical) {
        return Err(bad_request("thresholds must be in the range (0, 1]"));
    }
    if warn > critical {
        return Err(bad_request("warn_threshold must be <= critical_threshold"));
    }
    Ok((warn, critical))
}

fn classify(used_fraction: Option<f64>, warn: f64, critical: f64) -> WarnLevel {
    match used_fraction {
        None => WarnLevel::None,
        Some(f) if f >= 1.0 => WarnLevel::Exceeded,
        Some(f) if f >= critical => WarnLevel::Critical,
        Some(f) if f >= warn => WarnLevel::Warning,
        Some(_) => WarnLevel::Ok,
    }
}

/// Compute the rolling-window spend and budget level for one account, off the
/// shared `usage_events` table. Mirrors the Claude route exactly.
async fn account_usage(state: &AppState, acct: &GrokAccount) -> anyhow::Result<AccountUsage> {
    let since = match acct.budget_window_hours {
        Some(h) if h > 0 => now_ms() - (h as i64) * 3_600_000,
        _ => 0,
    };
    let rows = state.db.account_usage_since(&acct.id, since).await?;

    let mut total_tokens = 0i64;
    let mut est_cost = 0.0f64;
    let mut turns = 0i64;
    for r in &rows {
        total_tokens += r.total_tokens;
        turns += r.turns;
        est_cost += usage_cost(
            r.model.as_deref(),
            r.input_tokens,
            r.output_tokens,
            r.cache_read_tokens,
            r.cache_creation_tokens,
        );
    }

    let has_window = matches!(acct.budget_window_hours, Some(h) if h > 0);
    let token_frac = acct
        .budget_limit_tokens
        .filter(|l| *l > 0)
        .map(|l| total_tokens as f64 / l as f64);
    let cost_frac = acct
        .budget_limit_usd
        .filter(|l| *l > 0.0)
        .map(|l| est_cost / l);
    let used_fraction = if has_window {
        match (token_frac, cost_frac) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    } else {
        None
    };

    let level = classify(used_fraction, acct.warn_threshold, acct.critical_threshold);

    Ok(AccountUsage {
        total_tokens,
        est_cost_usd: est_cost,
        turns,
        used_fraction,
        level,
    })
}

async fn to_view(state: &AppState, acct: GrokAccount) -> anyhow::Result<AccountView> {
    let usage = account_usage(state, &acct).await?;
    Ok(AccountView {
        authenticated: is_authenticated(&acct),
        id: acct.id,
        name: acct.name,
        kind: acct.kind,
        config_dir: acct.config_dir,
        budget_window_hours: acct.budget_window_hours,
        budget_limit_usd: acct.budget_limit_usd,
        budget_limit_tokens: acct.budget_limit_tokens,
        warn_threshold: acct.warn_threshold,
        critical_threshold: acct.critical_threshold,
        created_at: acct.created_at,
        updated_at: acct.updated_at,
        usage,
    })
}

// ── Handlers ─────────────────────────────────────────────────────────

async fn list_accounts(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let accounts = match state.db.list_grok_accounts().await {
        Ok(a) => a,
        Err(e) => return Err(server_error(e)),
    };
    let mut out = Vec::with_capacity(accounts.len());
    for acct in accounts {
        match to_view(&state, acct).await {
            Ok(v) => out.push(v),
            Err(e) => return Err(server_error(e)),
        }
    }
    Ok(Json(out))
}

async fn create_account(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateAccountBody>,
) -> impl IntoResponse {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(bad_request("name is required"));
    }
    let kind = if body.kind.is_empty() {
        "device".to_string()
    } else {
        body.kind.clone()
    };
    if !valid_kind(&kind) {
        return Err(bad_request("kind must be 'device' or 'api_key'"));
    }
    let credential = if kind == "api_key" {
        let key = body.credential.as_deref().unwrap_or("").trim().to_string();
        if key.is_empty() {
            return Err(bad_request("credential is required for an api_key account"));
        }
        key
    } else {
        DEVICE_MARKER.to_string()
    };

    let (warn, critical) = match normalize_thresholds(body.warn_threshold, body.critical_threshold)
    {
        Ok(t) => t,
        Err(e) => return Err(e),
    };

    // `gacc_<hex>` — deliberately free of `@` so it never collides with the
    // model-id account separator, and `g`-prefixed to set it apart from a
    // Claude `acc_` id at a glance.
    let id = format!("gacc_{}", uuid::Uuid::new_v4().simple());
    let config_dir = state
        .config
        .data_dir
        .join("grok-accounts")
        .join(&id)
        .to_string_lossy()
        .to_string();
    let now = now_ms();

    let new = NewGrokAccount {
        id,
        name,
        kind,
        credential,
        config_dir: Some(config_dir),
        budget_window_hours: body.budget_window_hours,
        budget_limit_usd: body.budget_limit_usd,
        budget_limit_tokens: body.budget_limit_tokens,
        warn_threshold: warn,
        critical_threshold: critical,
        created_at: now,
        updated_at: now,
    };

    match state.db.create_grok_account(new).await {
        Ok(acct) => match to_view(&state, acct).await {
            Ok(v) => Ok((StatusCode::CREATED, Json(v))),
            Err(e) => Err(server_error(e)),
        },
        Err(e) => Err(server_error(e)),
    }
}

async fn update_account(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<UpdateAccountBody>,
) -> impl IntoResponse {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(bad_request("name is required"));
    }
    let (warn, critical) = match normalize_thresholds(body.warn_threshold, body.critical_threshold)
    {
        Ok(t) => t,
        Err(e) => return Err(e),
    };

    // Only a non-empty credential replaces the stored one (api_key accounts);
    // a device account's marker is never overwritten from the UI.
    let credential = body
        .credential
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_string);

    let changes = GrokAccountChanges {
        name: Some(name),
        credential,
        budget_window_hours: Some(body.budget_window_hours),
        budget_limit_usd: Some(body.budget_limit_usd),
        budget_limit_tokens: Some(body.budget_limit_tokens),
        warn_threshold: Some(warn),
        critical_threshold: Some(critical),
        updated_at: Some(now_ms()),
    };

    match state.db.update_grok_account(&id, changes).await {
        Ok(Some(acct)) => match to_view(&state, acct).await {
            Ok(v) => Ok(Json(v)),
            Err(e) => Err(server_error(e)),
        },
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "account not found" })),
        )),
        Err(e) => Err(server_error(e)),
    }
}

async fn delete_account(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // Stop any in-flight login before removing the row / its GROK_HOME.
    GROK_LOGIN.cancel(&id).await;
    match state.db.delete_grok_account(&id).await {
        Ok(Some(config_dir)) => {
            if let Some(dir) = config_dir {
                let _ = std::fs::remove_dir_all(&dir);
            }
            Ok(StatusCode::NO_CONTENT)
        }
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "account not found" })),
        )),
        Err(e) => Err(server_error(e)),
    }
}

#[derive(Serialize)]
struct LoginStartResponse {
    url: String,
}

/// Begin a device login for an existing account: spawn `grok login
/// --device-auth` against the account's GROK_HOME and return the sign-in URL.
/// The account reads as `authenticated` once the browser sign-in completes.
async fn start_login(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let account = match state.db.get_grok_account(&id).await {
        Ok(Some(a)) => a,
        Ok(None) => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "account not found" })),
            ));
        }
        Err(e) => return Err(server_error(e)),
    };
    if account.kind != "device" {
        return Err(bad_request(
            "login is only available for device accounts; api_key accounts use a stored key",
        ));
    }
    let Some(config_dir) = account.config_dir.as_deref() else {
        return Err(server_error("account has no config_dir"));
    };
    match GROK_LOGIN.start(&id, config_dir).await {
        Ok(url) => Ok(Json(LoginStartResponse { url })),
        Err(e) => Err(bad_request(&format!("Grok login failed: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_thresholds_in_order() {
        assert_eq!(classify(None, 0.75, 0.90), WarnLevel::None);
        assert_eq!(classify(Some(0.50), 0.75, 0.90), WarnLevel::Ok);
        assert_eq!(classify(Some(0.75), 0.75, 0.90), WarnLevel::Warning);
        assert_eq!(classify(Some(0.90), 0.75, 0.90), WarnLevel::Critical);
        assert_eq!(classify(Some(1.0), 0.75, 0.90), WarnLevel::Exceeded);
    }

    #[test]
    fn normalize_thresholds_validates_range_and_order() {
        assert_eq!(normalize_thresholds(None, None).unwrap(), (0.75, 0.90));
        assert!(normalize_thresholds(Some(0.0), Some(0.9)).is_err());
        assert!(normalize_thresholds(Some(0.95), Some(0.80)).is_err());
    }

    #[test]
    fn valid_kind_accepts_only_known_kinds() {
        assert!(valid_kind("device"));
        assert!(valid_kind("api_key"));
        assert!(!valid_kind("oauth_token"));
        assert!(!valid_kind(""));
    }

    fn account(kind: &str, credential: &str, config_dir: Option<&str>) -> GrokAccount {
        GrokAccount {
            id: "gacc_x".into(),
            name: "n".into(),
            kind: kind.into(),
            credential: credential.into(),
            config_dir: config_dir.map(str::to_string),
            budget_window_hours: None,
            budget_limit_usd: None,
            budget_limit_tokens: None,
            warn_threshold: 0.75,
            critical_threshold: 0.90,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn is_authenticated_keys_off_kind() {
        // api_key: authenticated iff a key is set.
        assert!(is_authenticated(&account("api_key", "xai-123", None)));
        assert!(!is_authenticated(&account("api_key", "", None)));
        // device: authenticated only when auth.json exists (no dir → false).
        assert!(!is_authenticated(&account("device", "device", None)));
        assert!(!is_authenticated(&account(
            "device",
            "device",
            Some("/nonexistent/xyz")
        )));
    }
}
