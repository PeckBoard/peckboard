use std::sync::{Arc, RwLock};

use crate::auth::rate_limit::RateLimiter;
use crate::config::Config;
use crate::db::Db;
use crate::plugin::builtin::BuiltinPluginRegistry;
use crate::plugin::manager::PluginManager;
use crate::provider::manager::SessionManager;
use crate::provider::registry::ProviderRegistry;
use crate::repeating::{RepeatingTaskManager, RunAuditor};
use crate::service::mcp_server::McpTokenRegistry;
use crate::service::push::PushService;
use crate::service::tls::{TlsMaterial, TlsSource, certified_key, material_info};
use crate::ws::broadcaster::Broadcaster;

/// Snapshot of what the HTTPS listener is currently serving, for the
/// settings API and the startup-failure announcement.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct TlsStatus {
    pub source: Option<TlsSource>,
    pub sans: Vec<String>,
    pub not_after: Option<chrono::DateTime<chrono::Utc>>,
    pub last_error: Option<String>,
    pub https_enabled: bool,
    /// Whether the HTTPS listener actually bound at startup. Material
    /// swapped in later only turns HTTPS back on if the socket is there,
    /// so the settings routes consult this before flipping
    /// `https_enabled`.
    pub listener_bound: bool,
}

/// Hot-swappable TLS key material. The HTTPS listener is built once at
/// startup with a `ResolvesServerCert` backed by this state; `load_from`
/// swaps the key it hands out on the next handshake, so a new upload or
/// regenerated self-signed cert takes effect with no restart and no
/// listener rebind.
pub struct TlsState {
    current: RwLock<Option<Arc<rustls::sign::CertifiedKey>>>,
    pub status: RwLock<TlsStatus>,
}

impl Default for TlsState {
    fn default() -> Self {
        Self::new()
    }
}

impl TlsState {
    pub fn new() -> Self {
        Self {
            current: RwLock::new(None),
            status: RwLock::new(TlsStatus::default()),
        }
    }

    /// Parse, validate, and swap in `material`. Updates `status` either
    /// way — on success with the new source/sans/expiry, on failure with
    /// `last_error` — so a failed reload is visible without restarting.
    pub fn load_from(
        &self,
        data_dir: &std::path::Path,
        material: &TlsMaterial,
    ) -> anyhow::Result<()> {
        match certified_key(material) {
            Ok(key) => {
                *self.current.write().unwrap() = Some(key);
                let info = material_info(data_dir, material);
                let mut status = self.status.write().unwrap();
                status.source = Some(info.source);
                status.sans = info.sans;
                status.not_after = info.not_after;
                status.last_error = None;
                Ok(())
            }
            Err(e) => {
                self.status.write().unwrap().last_error = Some(format!("{e:#}"));
                Err(e)
            }
        }
    }

    pub fn set_error(&self, err: &str) {
        self.status.write().unwrap().last_error = Some(err.to_string());
    }

    pub fn set_listener_bound(&self, bound: bool) {
        self.status.write().unwrap().listener_bound = bound;
    }

    pub fn set_https_enabled(&self, enabled: bool) {
        self.status.write().unwrap().https_enabled = enabled;
    }

    pub fn snapshot(&self) -> TlsStatus {
        self.status.read().unwrap().clone()
    }
}

/// `rustls` cert resolver backed by `TlsState`. Returns the current key
/// on every handshake, or `None` when no cert has loaded successfully —
/// which `rustls` turns into a clean handshake failure, not a panic or a
/// dead listener.
pub struct TlsCertResolver(pub Arc<TlsState>);

impl std::fmt::Debug for TlsCertResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsCertResolver").finish_non_exhaustive()
    }
}

impl rustls::server::ResolvesServerCert for TlsCertResolver {
    fn resolve(
        &self,
        _client_hello: rustls::server::ClientHello,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        self.0.current.read().unwrap().clone()
    }
}

pub struct AppState {
    pub config: Config,
    pub db: Db,
    pub plugins: Arc<PluginManager>,
    /// Catalog of statically-linked Rust plugins (currently `claude-code`
    /// and `mock`). Surfaces in the Settings UI via `/api/plugins`.
    pub builtin_plugins: Arc<BuiltinPluginRegistry>,
    pub jwt_secret: Vec<u8>,
    /// AES-256-GCM key for the SSH key vault (`service::ssh_keys`).
    /// Server-held, not derived from a user password -- see that module's
    /// docs for why.
    pub ssh_vault_key: Vec<u8>,
    /// AES-256-GCM key for TOTP secrets (`auth::mfa::vault`). Server-held,
    /// not derived from a user password — verification happens after the
    /// password has left memory.
    pub mfa_vault_key: Vec<u8>,
    pub login_limiter: RateLimiter,
    /// Per-user throttle on `POST /api/auth/change-password`. Keyed by
    /// user id so a compromised token can't flip the password in a
    /// tight loop (lockout DoS against the legitimate user).
    pub password_change_limiter: RateLimiter<String>,
    pub broadcaster: Arc<Broadcaster>,
    pub provider_registry: Arc<ProviderRegistry>,
    pub session_manager: SessionManager,
    pub repeating_task_manager: RepeatingTaskManager,
    /// Independent watchdog that observes scheduler-initiated repeating-
    /// task runs and refuses dispatch / kill-switches the task if the
    /// schedule's minimum-gap invariant is violated. Cheap to clone.
    pub run_auditor: RunAuditor,
    pub mcp_tokens: McpTokenRegistry,
    /// In-memory unlock registry + short-lived decrypted-value cache for
    /// user-defined encrypted env vars. See `service::env_vars`.
    pub env_unlock: std::sync::Arc<crate::service::env_vars::EnvUnlockRegistry>,
    pub push_service: PushService,
    /// One-time, plugin-scoped tickets for the restricted plugin-page
    /// WebSocket (`/ws/plugin-ui`). See [`crate::ws::plugin_ui`].
    pub plugin_ws_tickets: crate::ws::plugin_ui::PluginWsTickets,
    pub tls: Arc<TlsState>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::tls;
    use tempfile::TempDir;

    // `rustls::server::ClientHello` has no public constructor outside a
    // real handshake, so these tests read `TlsState.current` directly
    // (accessible: `tests` is a descendant of the module that declares
    // it private) rather than going through `TlsCertResolver::resolve`,
    // whose body is just a pass-through to that field.

    fn resolved_cert_der(state: &TlsState) -> Vec<u8> {
        let key = state
            .current
            .read()
            .unwrap()
            .clone()
            .expect("resolver should return a key after a successful load");
        key.cert[0].as_ref().to_vec()
    }

    #[test]
    fn new_tls_state_resolver_returns_none() {
        let state = TlsState::new();
        assert!(state.current.read().unwrap().is_none());
        assert!(state.snapshot().last_error.is_none());
        assert!(state.snapshot().source.is_none());
    }

    #[test]
    fn load_from_updates_status_and_resolver() {
        let tmp = TempDir::new().unwrap();
        let material = tls::ensure_certs(tmp.path()).unwrap();

        let state = TlsState::new();
        state.load_from(tmp.path(), &material).unwrap();

        let status = state.snapshot();
        assert_eq!(status.source, Some(TlsSource::SelfSigned));
        assert!(!status.sans.is_empty());
        assert!(status.last_error.is_none());
        assert!(state.current.read().unwrap().is_some());
    }

    #[test]
    fn load_from_failure_sets_last_error_without_touching_resolver() {
        let tmp = TempDir::new().unwrap();
        let bogus = TlsMaterial {
            cert_path: tmp.path().join("no-such-cert.pem"),
            key_path: tmp.path().join("no-such-key.pem"),
            source: TlsSource::SelfSigned,
        };

        let state = TlsState::new();
        assert!(state.load_from(tmp.path(), &bogus).is_err());

        let status = state.snapshot();
        assert!(status.last_error.is_some());
        assert!(state.current.read().unwrap().is_none());
    }

    #[test]
    fn load_from_hot_swaps_the_resolved_key() {
        let tmp1 = TempDir::new().unwrap();
        let material1 = tls::ensure_certs(tmp1.path()).unwrap();
        let tmp2 = TempDir::new().unwrap();
        let material2 = tls::ensure_certs(tmp2.path()).unwrap();

        let state = TlsState::new();

        state.load_from(tmp1.path(), &material1).unwrap();
        let first_der = resolved_cert_der(&state);

        state.load_from(tmp2.path(), &material2).unwrap();
        let second_der = resolved_cert_der(&state);

        assert_ne!(
            first_der, second_der,
            "a second load_from must swap in a different key, not keep serving the first"
        );
    }

    #[test]
    fn set_https_enabled_and_set_error_update_status() {
        let state = TlsState::new();
        state.set_https_enabled(true);
        assert!(state.snapshot().https_enabled);
        state.set_error("boom");
        assert_eq!(state.snapshot().last_error.as_deref(), Some("boom"));
    }

    /// A key corrupted after generation (bad edit, partial deploy) no
    /// longer reaches the resolver at all: `ensure_certs` re-validates the
    /// pair it found on disk and mints a fresh one when it doesn't hold
    /// together, so the next start serves working material instead of a
    /// certificate whose key can't sign for it.
    #[test]
    fn a_corrupted_key_is_replaced_by_the_next_ensure_certs() {
        let tmp = TempDir::new().unwrap();
        let material = tls::ensure_certs(tmp.path()).unwrap();
        std::fs::write(&material.key_path, "not actually a key").unwrap();

        let state = TlsState::new();
        let reloaded = tls::ensure_certs(tmp.path()).unwrap();
        state
            .load_from(tmp.path(), &reloaded)
            .expect("the corrupt pair must have been regenerated");
        assert!(state.current.read().unwrap().is_some());
        assert!(state.snapshot().last_error.is_none());
    }

    /// Mirrors main.rs's startup sequence for a failure `ensure_certs`
    /// can't heal (an unreadable certs directory, material handed in from
    /// elsewhere): `load_from` fails, HTTPS stays off, the banner is
    /// raised, and the next healthy start clears that same id.
    #[tokio::test]
    async fn a_failed_load_leaves_https_disabled_and_announces_failure() {
        let tmp = TempDir::new().unwrap();
        let db = Db::in_memory().unwrap();
        let state = TlsState::new();

        let missing = TlsMaterial {
            cert_path: tmp.path().join("no-such-cert.pem"),
            key_path: tmp.path().join("no-such-key.pem"),
            source: TlsSource::SelfSigned,
        };
        let err = state
            .load_from(tmp.path(), &missing)
            .expect_err("material that isn't there must fail load_from");
        tls::announce_failure(&db, &format!("{err:#}"))
            .await
            .unwrap();

        assert!(state.snapshot().last_error.is_some());
        assert!(state.current.read().unwrap().is_none());
        let all = db.list_announcements().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, tls::TLS_FAILURE_ANNOUNCEMENT_ID);

        // A subsequent healthy start must clear the same announcement id.
        let healthy = tls::regenerate_self_signed(tmp.path()).unwrap();
        state.load_from(tmp.path(), &healthy).unwrap();
        tls::clear_failure_announcement(&db).await.unwrap();
        assert!(db.list_announcements().await.unwrap().is_empty());
        assert!(state.current.read().unwrap().is_some());
        assert!(state.snapshot().last_error.is_none());
    }
}
