//! Admin settings for the voice Assistant, stored in the core settings KV
//! (`core.settings` / `app`, key [`SETTINGS_KEY`]) like the link-preview
//! settings. Missing or unreadable values fall back to the defaults.

use serde::{Deserialize, Serialize};

use crate::db::Db;

/// Key under the core settings KV.
pub const SETTINGS_KEY: &str = "assistant";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantSettings {
    /// Switch the user's screen to a topic's session or project when the
    /// topic is added or focused. Off by default: the topic ring tracks what
    /// the conversation is about, `show_view` changes what the user is
    /// looking at, and tying the two together yanked the user off the
    /// Assistant page every time they clicked a topic. When off, the
    /// Assistant navigates only when asked, and a turn started by a topic
    /// focus may not call `show_view` (see `handlers::voice_nav`).
    #[serde(default)]
    pub auto_show_on_topic: bool,
}

pub async fn load(db: &Db) -> AssistantSettings {
    use crate::routes::settings::{SETTINGS_COLLECTION, SETTINGS_NS};
    let db = db.clone();
    let raw = tokio::task::spawn_blocking(move || {
        db.plugin_store_get_blocking(SETTINGS_NS, SETTINGS_COLLECTION, SETTINGS_KEY)
    })
    .await;
    match raw {
        Ok(Ok(Some(json))) => serde_json::from_str(&json).unwrap_or_default(),
        _ => AssistantSettings::default(),
    }
}

pub async fn save(db: &Db, settings: &AssistantSettings) -> anyhow::Result<()> {
    use crate::routes::settings::{SETTINGS_COLLECTION, SETTINGS_NS};
    let db = db.clone();
    let value = serde_json::to_string(settings)?;
    tokio::task::spawn_blocking(move || {
        db.plugin_store_put_blocking(SETTINGS_NS, SETTINGS_COLLECTION, SETTINGS_KEY, &value)
    })
    .await??;
    Ok(())
}
