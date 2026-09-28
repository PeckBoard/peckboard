//! Per-provider base system prompt: the standing text every session of a
//! provider starts from, before any repeating-task suffix, caveman style, or
//! session/card custom prompt is layered on.
//!
//! Default = the provider crate's own prompt text (Claude's `ask_user` /
//! directory rules; empty for everyone else) followed by the shared
//! [`crate::provider::WORKING_STYLE`]. The user can replace it wholesale per
//! provider from Settings → Providers & Accounts; overrides live in the
//! plugin store under `core.settings`/`app`/[`PROVIDER_PROMPTS_KEY`] as
//! `{"<provider_id>": "<text>", ...}`.

use std::collections::BTreeMap;

use crate::db::Db;
use crate::routes::settings::{SETTINGS_COLLECTION, SETTINGS_NS};

/// Plugin-store key holding the per-provider base-prompt overrides.
pub const PROVIDER_PROMPTS_KEY: &str = "provider_prompts";

/// The full default base prompt for `provider_id`: the crate's
/// provider-specific text (if any) then the shared working-style rules —
/// the same order the Claude CLI sees them in (`--append-system-prompt`).
pub fn default_base_prompt(provider_id: &str) -> String {
    let specific = crate::plugin::crates::hooks_for(provider_id)
        .and_then(|h| h.base_prompt)
        .map(|f| f())
        .unwrap_or("");
    format!("{specific}{}", crate::provider::WORKING_STYLE)
}

/// Every stored override, keyed by provider id. Empty on missing/parse error.
pub async fn overrides(db: &Db) -> BTreeMap<String, String> {
    let db = db.clone();
    let raw = tokio::task::spawn_blocking(move || {
        db.plugin_store_get_blocking(SETTINGS_NS, SETTINGS_COLLECTION, PROVIDER_PROMPTS_KEY)
    })
    .await;
    match raw {
        Ok(Ok(Some(json))) => serde_json::from_str(&json).unwrap_or_default(),
        _ => BTreeMap::new(),
    }
}

/// The user's override for `provider_id`, if one is set (never blank).
pub async fn override_for(db: &Db, provider_id: &str) -> Option<String> {
    overrides(db)
        .await
        .remove(provider_id)
        .filter(|s| !s.trim().is_empty())
}

/// Set (`Some(non-blank)`) or clear (`None` / blank) `provider_id`'s override.
pub async fn set_override(db: &Db, provider_id: &str, text: Option<String>) -> anyhow::Result<()> {
    let mut all = overrides(db).await;
    match text.filter(|s| !s.trim().is_empty()) {
        Some(t) => {
            all.insert(provider_id.to_string(), t);
        }
        None => {
            all.remove(provider_id);
        }
    }
    let value = serde_json::to_string(&all)?;
    let db = db.clone();
    tokio::task::spawn_blocking(move || {
        db.plugin_store_put_blocking(
            SETTINGS_NS,
            SETTINGS_COLLECTION,
            PROVIDER_PROMPTS_KEY,
            &value,
        )
    })
    .await??;
    Ok(())
}
