//! Admin settings for the voice Assistant, stored in the core settings KV
//! (`core.settings` / `app`, key [`SETTINGS_KEY`]) like the link-preview
//! settings. Missing or unreadable values fall back to the defaults.

use serde::{Deserialize, Serialize};

use crate::db::Db;

/// Key under the core settings KV.
pub const SETTINGS_KEY: &str = "assistant";

/// Allowed ranges of the memory settings (inclusive).
pub const RETENTION_HOURS: (u32, u32) = (1, 168);
pub const MAX_ENTRIES: (u32, u32) = (5, 200);
pub const CLEAR_DELAY_MINUTES: (u32, u32) = (0, 60);
pub const IDLE_TOPIC_END_MINUTES: (u32, u32) = (5, 240);

/// When the Assistant's context is cleared automatically
/// (`service::assistant_memory::lifecycle`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoClear {
    /// After a topic is closed or ends by inactivity, once the delay has
    /// passed and the conversation is quiet.
    #[default]
    AfterTopic,
    /// Never: the conversation only grows (until the user clears it).
    Off,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Keep short facts across topic boundaries (`assistant_memories`).
    /// Off: nothing is written and no Recent Memory block is rendered.
    #[serde(default = "default_memory_enabled")]
    pub memory_enabled: bool,
    /// How long a memory lives, in hours ([`RETENTION_HOURS`]).
    #[serde(default = "default_retention_hours")]
    pub retention_hours: u32,
    /// Most memories kept; the oldest are pruned ([`MAX_ENTRIES`]).
    #[serde(default = "default_max_entries")]
    pub max_entries: u32,
    #[serde(default)]
    pub auto_clear: AutoClear,
    /// Minutes between a topic ending and the automatic clear
    /// ([`CLEAR_DELAY_MINUTES`]).
    #[serde(default = "default_clear_delay_minutes")]
    pub clear_delay_minutes: u32,
    /// Minutes without a user utterance after which the current topic
    /// counts as ended ([`IDLE_TOPIC_END_MINUTES`]).
    #[serde(default = "default_idle_topic_end_minutes")]
    pub idle_topic_end_minutes: u32,
}

fn default_memory_enabled() -> bool {
    true
}
fn default_retention_hours() -> u32 {
    36
}
fn default_max_entries() -> u32 {
    40
}
fn default_clear_delay_minutes() -> u32 {
    2
}
fn default_idle_topic_end_minutes() -> u32 {
    30
}

impl Default for AssistantSettings {
    fn default() -> Self {
        Self {
            auto_show_on_topic: false,
            memory_enabled: default_memory_enabled(),
            retention_hours: default_retention_hours(),
            max_entries: default_max_entries(),
            auto_clear: AutoClear::default(),
            clear_delay_minutes: default_clear_delay_minutes(),
            idle_topic_end_minutes: default_idle_topic_end_minutes(),
        }
    }
}

impl AssistantSettings {
    /// `Err(message)` naming the first field outside its range.
    pub fn validate(&self) -> Result<(), String> {
        for (field, value, (lo, hi)) in [
            ("retention_hours", self.retention_hours, RETENTION_HOURS),
            ("max_entries", self.max_entries, MAX_ENTRIES),
            (
                "clear_delay_minutes",
                self.clear_delay_minutes,
                CLEAR_DELAY_MINUTES,
            ),
            (
                "idle_topic_end_minutes",
                self.idle_topic_end_minutes,
                IDLE_TOPIC_END_MINUTES,
            ),
        ] {
            if !(lo..=hi).contains(&value) {
                return Err(format!("{field} must be between {lo} and {hi}"));
            }
        }
        Ok(())
    }

    /// `self` with the keys of `patch` (a JSON object) laid over it, so a
    /// client that only knows some fields can't reset the others. →
    /// `Err(message)` for a malformed or out-of-range value.
    pub fn merged(&self, patch: &serde_json::Value) -> Result<Self, String> {
        let Some(patch) = patch.as_object() else {
            return Err("settings must be a JSON object".into());
        };
        for (field, (lo, hi)) in [
            ("retention_hours", RETENTION_HOURS),
            ("max_entries", MAX_ENTRIES),
            ("clear_delay_minutes", CLEAR_DELAY_MINUTES),
            ("idle_topic_end_minutes", IDLE_TOPIC_END_MINUTES),
        ] {
            if patch.get(field).is_some_and(|v| !v.is_u64()) {
                return Err(format!(
                    "{field} must be a whole number between {lo} and {hi}"
                ));
            }
        }
        if patch
            .get("auto_clear")
            .is_some_and(|v| !matches!(v.as_str(), Some("after_topic" | "off")))
        {
            return Err("auto_clear must be \"after_topic\" or \"off\"".into());
        }
        let mut value = serde_json::to_value(self).map_err(|e| e.to_string())?;
        if let Some(obj) = value.as_object_mut() {
            for (k, v) in patch {
                obj.insert(k.clone(), v.clone());
            }
        }
        let next: Self =
            serde_json::from_value(value).map_err(|e| format!("invalid settings: {e}"))?;
        next.validate()?;
        Ok(next)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_fill_missing_fields_and_ranges_are_enforced() {
        // A value saved by 0.1.115 (only the topic-screen flag) keeps it and
        // gets the memory defaults.
        let old: AssistantSettings =
            serde_json::from_str(r#"{"auto_show_on_topic": true}"#).unwrap();
        assert!(old.auto_show_on_topic);
        assert!(old.memory_enabled);
        assert_eq!(
            (
                old.retention_hours,
                old.max_entries,
                old.clear_delay_minutes
            ),
            (36, 40, 2)
        );
        assert_eq!(old.idle_topic_end_minutes, 30);
        assert_eq!(old.auto_clear, AutoClear::AfterTopic);

        // A patch only overrides its own keys.
        let next = old
            .merged(&serde_json::json!({"auto_clear": "off", "clear_delay_minutes": 0}))
            .unwrap();
        assert!(next.auto_show_on_topic);
        assert_eq!(next.auto_clear, AutoClear::Off);
        assert_eq!(next.clear_delay_minutes, 0);

        for (patch, field) in [
            (serde_json::json!({"retention_hours": 0}), "retention_hours"),
            (
                serde_json::json!({"retention_hours": 169}),
                "retention_hours",
            ),
            (serde_json::json!({"max_entries": 4}), "max_entries"),
            (serde_json::json!({"max_entries": 201}), "max_entries"),
            (
                serde_json::json!({"clear_delay_minutes": 61}),
                "clear_delay_minutes",
            ),
            (
                serde_json::json!({"idle_topic_end_minutes": 4}),
                "idle_topic_end_minutes",
            ),
            (
                serde_json::json!({"idle_topic_end_minutes": 241}),
                "idle_topic_end_minutes",
            ),
            (
                serde_json::json!({"retention_hours": -1}),
                "retention_hours",
            ),
            (serde_json::json!({"auto_clear": "sometimes"}), "auto_clear"),
        ] {
            let err = old.merged(&patch).unwrap_err();
            assert!(err.contains(field), "{patch}: {err}");
        }
    }
}
