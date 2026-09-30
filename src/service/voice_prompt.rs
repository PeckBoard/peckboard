//! The voice assistant's editable system prompt.
//!
//! Every change appends a row to `voice_prompt_versions`; the newest row is
//! active. With no rows, or when the newest row is a reset
//! (`source = 'default'`), the built-in [`VOICE_SYSTEM_PROMPT`] is active —
//! so a user who never customized, or who reset, keeps getting whatever
//! default the running release ships. A reset row's `content` is only a
//! snapshot of the built-in at reset time, for history diffs.
//!
//! A save rewrites the voice session's stored `system_prompt` right away and
//! asks its live agent process to wind down after the current turn: the CLI
//! reads the system prompt only at spawn, so the next utterance spawns a
//! fresh process (resuming the same conversation) with the new prompt, and
//! an in-flight turn is never cut short.

use serde::Serialize;
use similar::{ChangeTag, TextDiff};

use crate::db::Db;
use crate::db::models::VoicePromptVersion;
use crate::provider::registry::ProviderRegistry;
use crate::service::voice_relay::{VOICE_EXPERT_KIND, VOICE_SYSTEM_PROMPT};

pub const SOURCE_DEFAULT: &str = "default";
pub const SOURCE_USER: &str = "user";
pub const SOURCE_ASSISTANT: &str = "assistant";

/// Longest prompt accepted, in characters.
pub const MAX_CHARS: usize = 50_000;

/// History rows returned / scanned.
const HISTORY_LIMIT: i64 = 500;

/// The prompt the voice session runs with right now.
#[derive(Serialize, Debug, Clone)]
pub struct ActivePrompt {
    pub content: String,
    /// `default` | `user` | `assistant`.
    pub source: String,
    pub is_default: bool,
    /// When the active version was saved; `None` = never customized.
    pub updated_at: Option<String>,
    pub default_content: String,
}

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiffStats {
    pub added: usize,
    pub removed: usize,
}

#[derive(Serialize, Debug, Clone)]
pub struct HistoryEntry {
    pub id: String,
    pub source: String,
    pub note: Option<String>,
    pub created_at: String,
    pub created_by: Option<String>,
    pub diff_stats: DiffStats,
}

#[derive(Serialize, Debug, Clone)]
pub struct VersionDetail {
    pub id: String,
    pub source: String,
    pub note: Option<String>,
    pub created_at: String,
    pub created_by: Option<String>,
    pub content: String,
    /// Unified diff against the version before it (the built-in default for
    /// the first one).
    pub diff: String,
    pub diff_stats: DiffStats,
}

/// Result of a save or reset.
#[derive(Serialize, Debug, Clone)]
pub struct Change {
    pub id: String,
    pub diff_stats: DiffStats,
    pub diff: String,
    /// False when the content equals the active prompt (nothing saved).
    pub changed: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum PromptError {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

/// The active prompt. A reset row yields the live built-in, not its stored
/// snapshot.
pub async fn active(db: &Db) -> anyhow::Result<ActivePrompt> {
    let latest = db.list_voice_prompt_versions(1).await?.into_iter().next();
    Ok(match latest {
        Some(row) if row.source != SOURCE_DEFAULT => ActivePrompt {
            content: row.content,
            source: row.source,
            is_default: false,
            updated_at: Some(row.created_at),
            default_content: VOICE_SYSTEM_PROMPT.to_string(),
        },
        other => ActivePrompt {
            content: VOICE_SYSTEM_PROMPT.to_string(),
            source: SOURCE_DEFAULT.to_string(),
            is_default: true,
            updated_at: other.map(|r| r.created_at),
            default_content: VOICE_SYSTEM_PROMPT.to_string(),
        },
    })
}

/// Just the active prompt text.
pub async fn active_content(db: &Db) -> anyhow::Result<String> {
    Ok(active(db).await?.content)
}

pub fn diff_stats(old: &str, new: &str) -> DiffStats {
    let mut stats = DiffStats::default();
    for change in TextDiff::from_lines(old, new).iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => stats.added += 1,
            ChangeTag::Delete => stats.removed += 1,
            ChangeTag::Equal => {}
        }
    }
    stats
}

pub fn unified_diff(old: &str, new: &str, old_name: &str, new_name: &str) -> String {
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(old_name, new_name)
        .to_string()
}

/// Save `content` as the active prompt. `source` is `user` or `assistant`.
/// Saving text identical to the active prompt is a no-op (`changed: false`).
pub async fn save(
    db: &Db,
    content: &str,
    source: &str,
    note: Option<String>,
    created_by: Option<String>,
) -> Result<Change, PromptError> {
    let content = content.trim_end().to_string();
    if content.trim().is_empty() {
        return Err(PromptError::Invalid("the prompt can't be empty".into()));
    }
    let chars = content.chars().count();
    if chars > MAX_CHARS {
        return Err(PromptError::Invalid(format!(
            "the prompt is too long ({chars} > {MAX_CHARS} characters)"
        )));
    }
    debug_assert!(source == SOURCE_USER || source == SOURCE_ASSISTANT);
    record(db, content, source, note, created_by).await
}

/// Make the built-in default active again. Future built-in changes then
/// apply without another reset.
pub async fn reset(db: &Db, created_by: Option<String>) -> Result<Change, PromptError> {
    record(
        db,
        VOICE_SYSTEM_PROMPT.to_string(),
        SOURCE_DEFAULT,
        Some("Reset to default".into()),
        created_by,
    )
    .await
}

async fn record(
    db: &Db,
    content: String,
    source: &str,
    note: Option<String>,
    created_by: Option<String>,
) -> Result<Change, PromptError> {
    let before = active(db).await?;
    let note = note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
    // Nothing to do: same text, and a reset while already on the default.
    if before.content == content && (source != SOURCE_DEFAULT || before.is_default) {
        return Ok(Change {
            id: String::new(),
            diff_stats: DiffStats::default(),
            diff: String::new(),
            changed: false,
        });
    }
    let diff = unified_diff(&before.content, &content, "previous", "new");
    let stats = diff_stats(&before.content, &content);
    let row = VoicePromptVersion {
        id: uuid::Uuid::new_v4().to_string(),
        content,
        source: source.to_string(),
        note: note.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by: created_by.clone(),
    };
    let id = row.id.clone();
    db.insert_voice_prompt_version(row).await?;
    tracing::info!(
        version = %id,
        source,
        note = note.as_deref().unwrap_or(""),
        created_by = created_by.as_deref().unwrap_or(""),
        added = stats.added,
        removed = stats.removed,
        "voice prompt changed\n{diff}"
    );
    Ok(Change {
        id,
        diff_stats: stats,
        diff,
        changed: true,
    })
}

/// Newest-first history, each with line stats against the version before it.
pub async fn history(db: &Db) -> anyhow::Result<Vec<HistoryEntry>> {
    let rows = db.list_voice_prompt_versions(HISTORY_LIMIT).await?;
    Ok(rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let prev = rows
                .get(i + 1)
                .map_or(VOICE_SYSTEM_PROMPT, |p| p.content.as_str());
            HistoryEntry {
                id: row.id.clone(),
                source: row.source.clone(),
                note: row.note.clone(),
                created_at: row.created_at.clone(),
                created_by: row.created_by.clone(),
                diff_stats: diff_stats(prev, &row.content),
            }
        })
        .collect())
}

/// One version with its unified diff against the version before it.
pub async fn version(db: &Db, id: &str) -> anyhow::Result<Option<VersionDetail>> {
    let rows = db.list_voice_prompt_versions(HISTORY_LIMIT).await?;
    let Some(i) = rows.iter().position(|r| r.id == id) else {
        return Ok(None);
    };
    let row = &rows[i];
    let prev = rows
        .get(i + 1)
        .map_or(VOICE_SYSTEM_PROMPT, |p| p.content.as_str());
    Ok(Some(VersionDetail {
        id: row.id.clone(),
        source: row.source.clone(),
        note: row.note.clone(),
        created_at: row.created_at.clone(),
        created_by: row.created_by.clone(),
        content: row.content.clone(),
        diff: unified_diff(prev, &row.content, "previous", "this version"),
        diff_stats: diff_stats(prev, &row.content),
    }))
}

/// Bring the voice session's stored prompt in line with the active one. With
/// a `registry`, a changed prompt also winds the session's live agent down
/// after its current turn so the next turn spawns with the new prompt.
/// Returns true when the session's prompt was rewritten.
pub async fn apply_to_voice_session(
    db: &Db,
    registry: Option<&ProviderRegistry>,
) -> anyhow::Result<bool> {
    let Some(session) = db.find_expert_session(VOICE_EXPERT_KIND).await? else {
        return Ok(false);
    };
    let content = active_content(db).await?;
    if session.system_prompt.as_deref() == Some(content.as_str()) {
        return Ok(false);
    }
    db.set_session_system_prompt(&session.id, Some(content), None)
        .await?;
    if let Some(registry) = registry {
        crate::provider::manager::shutdown_after_turn_via_registry(registry, &session.id).await;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{NewFolder, NewSession};

    async fn voice_db() -> Db {
        let db = Db::in_memory().unwrap();
        db.create_folder(NewFolder {
            id: "f1".into(),
            name: "f".into(),
            path: "/tmp".into(),
            created_at: "now".into(),
        })
        .await
        .unwrap();
        db.create_session(NewSession {
            id: "voice".into(),
            name: "voice".into(),
            folder_id: "f1".into(),
            created_at: "now".into(),
            last_activity: "now".into(),
            is_expert: true,
            expert_kind: Some(VOICE_EXPERT_KIND.into()),
            system_prompt: Some(VOICE_SYSTEM_PROMPT.into()),
            ..Default::default()
        })
        .await
        .unwrap();
        db
    }

    #[tokio::test]
    async fn default_save_history_reset_and_session_apply() {
        let db = voice_db().await;

        // Empty table: the built-in default is active.
        let a = active(&db).await.unwrap();
        assert!(a.is_default);
        assert_eq!(a.content, VOICE_SYSTEM_PROMPT);
        assert!(a.updated_at.is_none());
        assert!(history(&db).await.unwrap().is_empty());

        // Validation.
        assert!(matches!(
            save(&db, "  \n", SOURCE_USER, None, None).await,
            Err(PromptError::Invalid(_))
        ));
        let huge = "x".repeat(MAX_CHARS + 1);
        assert!(matches!(
            save(&db, &huge, SOURCE_USER, None, None).await,
            Err(PromptError::Invalid(_))
        ));

        // Save → active + history + diff; the voice session follows.
        let custom = format!("{VOICE_SYSTEM_PROMPT}\nAlways be brief.");
        let change = save(
            &db,
            &custom,
            SOURCE_USER,
            Some("brief".into()),
            Some("u1".into()),
        )
        .await
        .unwrap();
        assert!(change.changed);
        assert!(change.diff_stats.added >= 1);
        assert!(change.diff.contains("+Always be brief."), "{}", change.diff);
        let a = active(&db).await.unwrap();
        assert!(!a.is_default);
        assert_eq!(a.source, SOURCE_USER);
        assert_eq!(a.content, custom);
        assert!(apply_to_voice_session(&db, None).await.unwrap());
        let s = db.get_session("voice").await.unwrap().unwrap();
        assert_eq!(s.system_prompt.as_deref(), Some(custom.as_str()));

        // Saving the same text again is a no-op.
        let same = save(&db, &custom, SOURCE_USER, None, None).await.unwrap();
        assert!(!same.changed);

        let h = history(&db).await.unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].note.as_deref(), Some("brief"));
        assert_eq!(h[0].diff_stats, change.diff_stats);
        let v = version(&db, &h[0].id).await.unwrap().unwrap();
        assert!(v.diff.contains("+Always be brief."));

        // Reset → built-in active again; the session follows.
        let r = reset(&db, None).await.unwrap();
        assert!(r.changed);
        let a = active(&db).await.unwrap();
        assert!(a.is_default && a.updated_at.is_some());
        assert_eq!(a.content, VOICE_SYSTEM_PROMPT);
        assert!(apply_to_voice_session(&db, None).await.unwrap());
        let s = db.get_session("voice").await.unwrap().unwrap();
        assert_eq!(s.system_prompt.as_deref(), Some(VOICE_SYSTEM_PROMPT));

        // A second reset while on the default does nothing.
        assert!(!reset(&db, None).await.unwrap().changed);
        assert_eq!(history(&db).await.unwrap().len(), 2);

        // A reset recorded by an older release (stale snapshot) still yields
        // the current built-in: later built-in changes flow to reset users.
        db.insert_voice_prompt_version(VoicePromptVersion {
            id: "old-reset".into(),
            content: "an older built-in".into(),
            source: SOURCE_DEFAULT.into(),
            note: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            created_by: None,
        })
        .await
        .unwrap();
        assert_eq!(active_content(&db).await.unwrap(), VOICE_SYSTEM_PROMPT);
    }
}
