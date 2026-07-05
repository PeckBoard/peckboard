//! `/api/claude-accounts` — manage the set of Claude/Anthropic credentials
//! the spawned `claude` CLI can run as.
//!
//! The "Default" account (host credentials) is implicit and never appears
//! here; a session uses an account by carrying `@<account_id>` on its model
//! id (see [`crate::provider::registry::split_model_account`]). The model
//! picker surfaces every account as `[Name] Model` through the Claude
//! provider's `dynamic_models`, so switching accounts is just picking a
//! differently-labelled model.
//!
//! Each row also reports its rolling-window budget status so the UI can warn
//! before a real Anthropic limit bites. The stored `credential` is never
//! returned — only a masked hint.

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
use crate::db::models::{ClaudeAccount, ClaudeAccountChanges, NewClaudeAccount};
use crate::provider::claude::oauth;
use crate::provider::claude::plan_usage;
use crate::routes::usage::cost::usage_cost;
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/claude-accounts",
            get(list_accounts).post(create_account),
        )
        .route(
            "/api/claude-accounts/login/start",
            axum::routing::post(start_login),
        )
        .route(
            "/api/claude-accounts/plan-usage",
            get(get_plan_usage).post(refresh_plan_usage),
        )
        .route(
            "/api/claude-accounts/{id}",
            axum::routing::put(update_account).delete(delete_account),
        )
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

/// A short-lived client for the Claude OAuth token endpoint.
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default()
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

/// How close an account is to its budget, mapped to a warn level the UI
/// renders as ok / warning / critical / exceeded. `none` when the account
/// has no budget configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum WarnLevel {
    None,
    Ok,
    Warning,
    Critical,
    Exceeded,
}

/// Rolling-window spend + budget status for one account.
#[derive(Debug, Clone, Serialize)]
struct AccountUsage {
    /// Tokens billed in the window (or all-time when no window is set).
    total_tokens: i64,
    /// Estimated USD cost of those tokens, priced via the usage cost model.
    est_cost_usd: f64,
    turns: i64,
    /// Fraction of the budget consumed (max of the token- and cost-budget
    /// fractions); `null` when no budget is configured.
    used_fraction: Option<f64>,
    level: WarnLevel,
}

/// One account as returned to the UI. The `credential` is never sent back;
/// `credential_hint` is a masked tail so the user can tell two keys apart.
#[derive(Debug, Clone, Serialize)]
struct AccountView {
    id: String,
    name: String,
    kind: String,
    credential_hint: String,
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

/// A pasted Claude login from the browser flow: the `code#state` string the
/// user copied plus the PKCE `verifier`/`state` issued by `login/start`. When
/// present on a create/update, the server exchanges it for the long-lived
/// access token and uses that as the credential — the token never touches the
/// browser.
#[derive(Debug, Deserialize)]
struct OAuthLogin {
    code: String,
    verifier: String,
    state: String,
}

#[derive(Debug, Deserialize)]
struct CreateAccountBody {
    name: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    credential: Option<String>,
    /// Browser login result; when set, takes precedence over `credential` and
    /// forces an `oauth_token` account.
    #[serde(default)]
    login: Option<OAuthLogin>,
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
    /// Empty / absent leaves the stored credential untouched so a rename or
    /// rebudget never has to round-trip the secret.
    #[serde(default)]
    credential: Option<String>,
    /// A fresh browser login to re-authenticate the account. When set, the
    /// exchanged token replaces the stored credential.
    #[serde(default)]
    login: Option<OAuthLogin>,
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

fn mask(credential: &str) -> String {
    let n = credential.chars().count();
    if n <= 4 {
        "••••".to_string()
    } else {
        let tail: String = credential.chars().skip(n - 4).collect();
        format!("••••{tail}")
    }
}

fn valid_kind(kind: &str) -> bool {
    matches!(kind, "api_key" | "oauth_token")
}

/// Thresholds must be ordered fractions in `(0, 1]` so the level mapping is
/// monotonic. Defaults (0.75 / 0.90) apply when the field is absent.
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

/// Map a budget-consumed fraction to a warn level. `None` (no budget) →
/// `None`; `>=1.0` → exceeded; then critical / warn thresholds in turn.
fn classify(used_fraction: Option<f64>, warn: f64, critical: f64) -> WarnLevel {
    match used_fraction {
        None => WarnLevel::None,
        Some(f) if f >= 1.0 => WarnLevel::Exceeded,
        Some(f) if f >= critical => WarnLevel::Critical,
        Some(f) if f >= warn => WarnLevel::Warning,
        Some(_) => WarnLevel::Ok,
    }
}

/// Compute the rolling-window spend and budget level for one account.
async fn account_usage(state: &AppState, acct: &ClaudeAccount) -> anyhow::Result<AccountUsage> {
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

    // Fraction of budget consumed: the worse of the token- and cost-budget
    // fractions (an account can blow either limit first). `None` when the
    // account has no window or no limits at all.
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

async fn to_view(state: &AppState, acct: ClaudeAccount) -> anyhow::Result<AccountView> {
    let usage = account_usage(state, &acct).await?;
    Ok(AccountView {
        credential_hint: mask(&acct.credential),
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

/// Begin a browser login: mint a PKCE challenge and return the authorize URL
/// plus the `verifier`/`state` the client echoes back on create/update. This
/// is the in-app stand-in for `claude setup-token`.
async fn start_login(State(_state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(oauth::start())
}

/// Cached subscription plan usage (the `claude /usage` buckets) for every
/// login, keyed by account id — `"default"` is the host's own login. A
/// background poller refreshes the cache every 30 minutes; this endpoint
/// never blocks on the network.
async fn get_plan_usage(State(_state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(plan_usage::snapshot())
}

/// Force an immediate refresh of every login's plan usage, then return the
/// updated snapshot. Used by the settings page's manual refresh.
async fn refresh_plan_usage(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    plan_usage::refresh_once(&state.db).await;
    Json(plan_usage::snapshot())
}
async fn list_accounts(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let accounts = match state.db.list_claude_accounts().await {
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
    // The credential comes from one of two paths: a finished browser login
    // (exchanged here, forcing an `oauth_token` account) or a pasted secret.
    let (kind, credential) = match body.login {
        Some(login) => {
            let token =
                match oauth::exchange(&http_client(), &login.code, &login.verifier, &login.state)
                    .await
                {
                    Ok(t) => t,
                    Err(e) => return Err(bad_request(&format!("Claude login failed: {e}"))),
                };
            ("oauth_token".to_string(), token)
        }
        None => {
            if !valid_kind(&body.kind) {
                return Err(bad_request("kind must be 'api_key' or 'oauth_token'"));
            }
            let credential = body.credential.as_deref().unwrap_or("").trim().to_string();
            if credential.is_empty() {
                return Err(bad_request("credential is required"));
            }
            (body.kind, credential)
        }
    };
    let (warn, critical) = match normalize_thresholds(body.warn_threshold, body.critical_threshold)
    {
        Ok(t) => t,
        Err(e) => return Err(e),
    };

    // `acc_<hex>` — deliberately free of `@` so it never collides with the
    // model-id account separator.
    let id = format!("acc_{}", uuid::Uuid::new_v4().simple());
    let config_dir = state
        .config
        .data_dir
        .join("claude-accounts")
        .join(&id)
        .to_string_lossy()
        .to_string();
    let now = now_ms();

    let new = NewClaudeAccount {
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

    match state.db.create_claude_account(new).await {
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

    // PUT semantics: the form always sends the full editable state, so
    // budgets are set verbatim (a `null` clears them). The credential is the
    // one exception — only replaced when a fresh browser login is exchanged or
    // a non-empty secret is supplied; otherwise the stored secret is kept.
    let credential = match body.login {
        Some(login) => {
            match oauth::exchange(&http_client(), &login.code, &login.verifier, &login.state).await
            {
                Ok(t) => Some(t),
                Err(e) => return Err(bad_request(&format!("Claude login failed: {e}"))),
            }
        }
        None => body
            .credential
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_string),
    };

    let changes = ClaudeAccountChanges {
        name: Some(name),
        credential,
        budget_window_hours: Some(body.budget_window_hours),
        budget_limit_usd: Some(body.budget_limit_usd),
        budget_limit_tokens: Some(body.budget_limit_tokens),
        warn_threshold: Some(warn),
        critical_threshold: Some(critical),
        updated_at: Some(now_ms()),
    };

    match state.db.update_claude_account(&id, changes).await {
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
    match state.db.delete_claude_account(&id).await {
        Ok(Some(config_dir)) => {
            // Best-effort cleanup of the isolated CLI state dir.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_thresholds_in_order() {
        // No budget → no warning regardless of spend.
        assert_eq!(classify(None, 0.75, 0.90), WarnLevel::None);
        // Below warn.
        assert_eq!(classify(Some(0.50), 0.75, 0.90), WarnLevel::Ok);
        // At/above warn but below critical.
        assert_eq!(classify(Some(0.75), 0.75, 0.90), WarnLevel::Warning);
        assert_eq!(classify(Some(0.89), 0.75, 0.90), WarnLevel::Warning);
        // At/above critical but below the cap.
        assert_eq!(classify(Some(0.90), 0.75, 0.90), WarnLevel::Critical);
        assert_eq!(classify(Some(0.99), 0.75, 0.90), WarnLevel::Critical);
        // At/over the cap.
        assert_eq!(classify(Some(1.0), 0.75, 0.90), WarnLevel::Exceeded);
        assert_eq!(classify(Some(2.5), 0.75, 0.90), WarnLevel::Exceeded);
    }

    #[test]
    fn mask_hides_all_but_last_four() {
        assert_eq!(mask("sk-ant-abcd1234"), "••••1234");
        // Short secrets reveal nothing.
        assert_eq!(mask("ab"), "••••");
        assert_eq!(mask(""), "••••");
    }

    #[test]
    fn normalize_thresholds_validates_range_and_order() {
        assert_eq!(normalize_thresholds(None, None).unwrap(), (0.75, 0.90));
        assert!(normalize_thresholds(Some(0.0), Some(0.9)).is_err());
        assert!(normalize_thresholds(Some(0.5), Some(1.5)).is_err());
        // warn must not exceed critical.
        assert!(normalize_thresholds(Some(0.95), Some(0.80)).is_err());
        assert_eq!(
            normalize_thresholds(Some(0.6), Some(0.8)).unwrap(),
            (0.6, 0.8)
        );
    }

    #[test]
    fn valid_kind_accepts_only_known_kinds() {
        assert!(valid_kind("api_key"));
        assert!(valid_kind("oauth_token"));
        assert!(!valid_kind("password"));
        assert!(!valid_kind(""));
    }
}
