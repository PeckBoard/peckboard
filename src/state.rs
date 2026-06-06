use crate::config::Config;
use crate::db::Db;

pub struct AppState {
    pub config: Config,
    pub db: Db,
    // Future: claude_manager, broadcaster, push, etc.
}
