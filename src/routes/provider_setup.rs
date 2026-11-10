//! `/api/provider-setup` — what the setup wizard and Settings need to take a
//! CLI provider from nothing to usable: whether its CLI is installed, whether
//! it is signed in, and a one-click install that needs no working agent.
//!
//! Sign-in itself mostly reuses the per-provider account routes. Only Cursor
//! has a login here, because it has no accounts (see
//! [`crate::accounts::cursor_login`]).
//!
//! Admin-only: installing software and minting logins are host-wide.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{get, post},
};
use serde::Serialize;

use crate::accounts::cli_install::{self, CLI_PROVIDERS, CliProvider, InstallJob, SignIn};
use crate::accounts::{codex_login, cursor_login, grok_login, kimi_login};
use crate::auth::middleware::{require_admin, require_auth};
use crate::plugin::settings::{PluginSettingsStore, SettingsSchema};
use crate::provider::turn;
use crate::service::mcp_server::command_check;
use crate::state::AppState;

/// Probing a CLI must never hang the wizard on a broken install.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/provider-setup", get(list_status))
        .route("/api/provider-setup/{id}", get(one_status))
        .route("/api/provider-setup/{id}/install", post(start_install))
        .route(
            "/api/provider-setup/cursor/login/start",
            post(start_cursor_login),
        )
        .route_layer(middleware::from_fn(require_admin))
        .route_layer(middleware::from_fn_with_state(state, require_auth))
}

type ApiError = (StatusCode, Json<serde_json::Value>);

fn api_error(status: StatusCode, msg: impl std::fmt::Display) -> ApiError {
    (
        status,
        Json(serde_json::json!({ "error": msg.to_string() })),
    )
}

/// The binary a provider spawns: its `cli_path` setting (the one every turn
/// uses) or `default`, resolved through the installer-location fallback so a
/// service PATH that predates the install still works.
pub(crate) async fn configured_cli_path(state: &AppState, plugin: &str, default: &str) -> String {
    let schema = state
        .plugins
        .settings_schema_for(plugin)
        .await
        .unwrap_or_else(|| SettingsSchema::new(vec![]));
    let store = PluginSettingsStore::new(plugin.to_string(), schema, state.db.clone());
    let configured = match store.load().await {
        Ok(settings) => settings
            .get("cli_path")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        Err(e) => {
            tracing::warn!("{plugin}: failed to load settings for cli_path: {e}");
            None
        }
    };
    turn::resolve_cli_path(
        configured.as_deref().unwrap_or(default),
        turn::COMMON_CLI_FALLBACK_DIRS,
    )
}

#[derive(Serialize)]
struct AccountRef {
    id: String,
    name: String,
}

#[derive(Serialize)]
struct ProviderStatus {
    id: &'static str,
    display_name: &'static str,
    /// The provider plugin is on, so its models can be offered.
    active: bool,
    /// The admin hid it in Settings; models stay out of the pickers.
    hidden: bool,
    installed: bool,
    version: Option<String>,
    signed_in: bool,
    /// The stored account that is signed in. `None` when the sign-in is the
    /// host's own CLI login, whose models carry no `@account` suffix.
    account: Option<AccountRef>,
    sign_in: SignIn,
    /// What Install runs, shown to the admin before it does.
    install_command: &'static str,
    auto_install: bool,
    install_job: Option<InstallJob>,
}

/// The first line a CLI prints for `--version`.
async fn cli_version(cli_path: &str) -> Option<String> {
    let output = run_probe(cli_path, &["--version"]).await?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

async fn run_probe(cli_path: &str, args: &[&str]) -> Option<std::process::Output> {
    let mut cmd =
        crate::sandbox::SandboxedTokioCommand::new(cli_path, &crate::sandbox::SpawnScope::none());
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let child = cmd.spawn().ok()?;
    tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output())
        .await
        .ok()?
        .ok()
}

/// Whether a CLI's default home holds a non-empty `auth.json`. The "Default"
/// account is whatever the CLI is signed into on the host.
fn host_auth_file(env_home: &str, default_dir: &str) -> bool {
    let dir = std::env::var_os(env_home)
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(default_dir)));
    dir.and_then(|d| std::fs::metadata(d.join("auth.json")).ok())
        .is_some_and(|m| m.len() > 0)
}

/// The sign-in state: is anything signed in, and which stored account if so.
/// A stored account wins over a host login, because it is the one the user
/// just set up here and it is what the default model should point at.
async fn sign_in_state(
    state: &AppState,
    id: &str,
    cli_path: &str,
    installed: bool,
) -> (bool, Option<AccountRef>) {
    let account = match id {
        "claude" => state.db.list_claude_accounts().await.ok().and_then(|a| {
            a.into_iter().next().map(|a| AccountRef {
                id: a.id,
                name: a.name,
            })
        }),
        "codex" => state.db.list_codex_accounts().await.ok().and_then(|a| {
            a.into_iter()
                .find(|a| codex_login::device_authenticated(a.config_dir.as_deref()))
                .map(|a| AccountRef {
                    id: a.id,
                    name: a.name,
                })
        }),
        "grok" => state.db.list_grok_accounts().await.ok().and_then(|a| {
            a.into_iter()
                .find(|a| {
                    if a.kind == "api_key" {
                        !a.credential.is_empty()
                    } else {
                        grok_login::device_authenticated(a.config_dir.as_deref())
                    }
                })
                .map(|a| AccountRef {
                    id: a.id,
                    name: a.name,
                })
        }),
        "kimi" => state.db.list_kimi_accounts().await.ok().and_then(|a| {
            a.into_iter()
                .find(|a| {
                    if a.kind == "api_key" {
                        !a.credential.is_empty()
                    } else {
                        kimi_login::device_authenticated(a.config_dir.as_deref())
                    }
                })
                .map(|a| AccountRef {
                    id: a.id,
                    name: a.name,
                })
        }),
        _ => None,
    };
    if account.is_some() {
        return (true, account);
    }
    let host = match id {
        "claude" if installed => match run_probe(cli_path, &["auth", "status", "--json"]).await {
            Some(output) => parse_claude_auth(&output.stdout),
            None => false,
        },
        "cursor" if installed => cursor_login::signed_in(cli_path).await,
        "codex" => host_auth_file("CODEX_HOME", ".codex"),
        "grok" => host_auth_file("GROK_HOME", ".grok"),
        _ => false,
    };
    (host, None)
}

fn parse_claude_auth(stdout: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(stdout)
        .ok()
        .and_then(|v| v.get("loggedIn")?.as_bool())
        .unwrap_or(false)
}

async fn status_for(state: &AppState, provider: &'static CliProvider) -> ProviderStatus {
    let cli_path = configured_cli_path(state, provider.id, provider.binary).await;
    let installed = command_check::check_command(&cli_path).found;
    let job = cli_install::job_snapshot(provider.id);
    let installing = job.as_ref().is_some_and(|j| j.running);
    let version = if installed && !installing {
        cli_version(&cli_path).await
    } else {
        None
    };
    let (signed_in, account) = sign_in_state(state, provider.id, &cli_path, installed).await;
    let active = state
        .provider_registry
        .list_providers()
        .await
        .iter()
        .any(|p| p.id == provider.id);
    let hidden = super::settings::hidden_providers_for_db(state.db.clone())
        .await
        .contains(provider.id);
    ProviderStatus {
        id: provider.id,
        display_name: provider.display_name,
        active,
        hidden,
        installed,
        version,
        signed_in,
        account,
        sign_in: provider.sign_in,
        install_command: if cli_install::auto_install_supported() {
            provider.install_script
        } else {
            provider.windows_install
        },
        auto_install: cli_install::auto_install_supported(),
        install_job: job,
    }
}

/// `GET /api/provider-setup` — every CLI provider, plus the providers with no
/// CLI to set up, which only need turning on.
async fn list_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let providers =
        futures_util::future::join_all(CLI_PROVIDERS.iter().map(|p| status_for(&state, p))).await;
    let registered = state.provider_registry.list_providers().await;
    let hidden = super::settings::hidden_providers_for_db(state.db.clone()).await;
    let other_providers: Vec<_> = crate::plugin::crates::provider_ids()
        .filter(|id| *id != "mock" && cli_install::find_provider(id).is_none())
        .map(|id| {
            serde_json::json!({
                "id": id,
                "display_name": crate::plugin::crates::display_name(id).unwrap_or(id),
                "active": registered.iter().any(|p| p.id == id),
                "hidden": hidden.contains(id),
            })
        })
        .collect();
    Json(serde_json::json!({
        "providers": providers,
        "other_providers": other_providers,
    }))
}

/// `GET /api/provider-setup/{id}` — one provider, for polling a running
/// install or an open sign-in dialog without probing all the others.
async fn one_status(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<ProviderStatus>, ApiError> {
    let provider = cli_install::find_provider(&id)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "unknown provider"))?;
    Ok(Json(status_for(&state, provider).await))
}

/// `POST /api/provider-setup/{id}/install` — run the provider's installer in
/// the background and turn the provider on if it works. Poll
/// [`one_status`] for progress.
async fn start_install(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let provider = cli_install::find_provider(&id)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "unknown provider"))?;
    if !cli_install::auto_install_supported() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            format!(
                "Run this yourself on the server: {}",
                provider.windows_install
            ),
        ));
    }
    if !cli_install::try_begin(provider.id) {
        return Err(api_error(
            StatusCode::CONFLICT,
            "An install is already running for this provider.",
        ));
    }
    tracing::info!(provider = provider.id, "provider setup: installing CLI");
    let task_state = state.clone();
    tokio::spawn(async move {
        let cli_path = configured_cli_path(&task_state, provider.id, provider.binary).await;
        let check = || {
            // Re-resolved on each check: a bare name has to be looked up again
            // now that the installer has run.
            command_check::check_command(&cli_path).found
                || command_check::check_command(provider.binary).found
        };
        let ok = cli_install::run(
            provider.id,
            provider.install_script,
            provider.home_dirs,
            check,
        )
        .await;
        if ok {
            super::plugins::activate_crate_plugin(&task_state, provider.id).await;
        } else {
            tracing::warn!(provider = provider.id, "provider setup: install failed");
        }
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(status_for(&state, provider).await),
    ))
}

/// `POST /api/provider-setup/cursor/login/start` — begin the host's Cursor
/// login and return the link to open. Polling [`one_status`] reports when it
/// lands.
async fn start_cursor_login(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    let cli_path = configured_cli_path(&state, "cursor", "cursor-agent").await;
    match cursor_login::CURSOR_LOGIN.start(&cli_path).await {
        Ok(url) => Ok(Json(serde_json::json!({ "url": url }))),
        Err(e) => {
            tracing::warn!("cursor login start failed: {e}");
            Err(api_error(StatusCode::BAD_REQUEST, e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_auth_status_reads_logged_in() {
        assert!(parse_claude_auth(
            br#"{"loggedIn":true,"authMethod":"claude.ai"}"#
        ));
        assert!(!parse_claude_auth(br#"{"loggedIn":false}"#));
        assert!(!parse_claude_auth(b""));
    }

    #[test]
    fn host_auth_needs_a_non_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let var = "PECKBOARD_TEST_PROVIDER_SETUP_HOME";
        // SAFETY: this test alone reads or writes this uniquely named variable.
        unsafe { std::env::set_var(var, dir.path()) };
        assert!(!host_auth_file(var, ".unused"));
        std::fs::write(dir.path().join("auth.json"), "").unwrap();
        assert!(!host_auth_file(var, ".unused"));
        std::fs::write(dir.path().join("auth.json"), "{}").unwrap();
        assert!(host_auth_file(var, ".unused"));
        // SAFETY: as above.
        unsafe { std::env::remove_var(var) };
    }

    #[test]
    fn installers_land_where_the_provider_will_look() {
        // Grok and Kimi install outside ~/.local/bin. If these leave the
        // fallback list, Install reports "can't be found" on a service PATH.
        for dir in ["~/.local/bin", "~/.grok/bin", "~/.kimi-code/bin"] {
            assert!(turn::COMMON_CLI_FALLBACK_DIRS.contains(&dir), "{dir}");
        }
    }
}
