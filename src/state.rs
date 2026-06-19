use std::sync::Arc;

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
use crate::ws::broadcaster::Broadcaster;

pub struct AppState {
    pub config: Config,
    pub db: Db,
    pub plugins: Arc<PluginManager>,
    /// Catalog of statically-linked Rust plugins (currently `claude-code`
    /// and `mock`). Surfaces in the Settings UI via `/api/plugins`.
    pub builtin_plugins: Arc<BuiltinPluginRegistry>,
    pub jwt_secret: Vec<u8>,
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
    pub push_service: PushService,
}
