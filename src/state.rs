use std::sync::Arc;

use crate::auth::rate_limit::RateLimiter;
use crate::config::Config;
use crate::db::Db;
use crate::plugin::manager::PluginManager;
use crate::provider::manager::SessionManager;
use crate::provider::registry::ProviderRegistry;
use crate::service::mcp_server::McpTokenRegistry;
use crate::service::push::PushService;
use crate::ws::broadcaster::Broadcaster;

pub struct AppState {
    pub config: Config,
    pub db: Db,
    pub plugins: Arc<PluginManager>,
    pub jwt_secret: Vec<u8>,
    pub login_limiter: RateLimiter,
    pub broadcaster: Arc<Broadcaster>,
    pub provider_registry: Arc<ProviderRegistry>,
    pub session_manager: SessionManager,
    pub mcp_tokens: McpTokenRegistry,
    pub push_service: PushService,
}
