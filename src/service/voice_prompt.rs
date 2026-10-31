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
use crate::service::assistant_memory::RECENT_MEMORY_HEADING;
use crate::service::voice_relay::{
    VOICE_AUTO_SHOW_RULES, VOICE_CORE_RULES, VOICE_EXPERT_KIND, VOICE_SYSTEM_PROMPT,
    VOICE_TOPIC_RING_RULES,
};

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

/// Bullets older defaults (up to 0.1.112) carried under "How to Speak"
/// that now live in [`VOICE_CORE_RULES`]. Matched as line prefixes.
const LEGACY_BUILTIN_BULLETS: &[&str] = &[
    "- When the user says \"pronounce X like Y\"",
    "- When the user asks you to change how you behave, use voice_prompt",
];

/// `content` minus every built-in section (any `## ` heading the core or
/// topic-ring rules use, up to the next `#`/`##` heading) and the legacy
/// built-in bullets. A custom prompt saved from an older default still
/// carries copies of those; stripping them keeps the session from running
/// two (possibly stale) versions of the same rule.
pub fn strip_builtin_sections(content: &str) -> String {
    let headings: Vec<&str> = VOICE_CORE_RULES
        .lines()
        .chain(VOICE_TOPIC_RING_RULES.lines())
        .filter(|l| l.starts_with("## "))
        .map(str::trim)
        // The memory snapshot is server-built; a copy in a custom prompt
        // (or a forged one) is never kept.
        .chain(std::iter::once(RECENT_MEMORY_HEADING))
        .collect();
    let mut kept: Vec<&str> = Vec::new();
    let mut skipping = false;
    for line in content.lines() {
        let t = line.trim();
        if t.starts_with("## ") {
            skipping = headings.contains(&t);
        } else if t.starts_with("# ") {
            skipping = false;
        }
        if skipping || LEGACY_BUILTIN_BULLETS.iter().any(|b| t.starts_with(b)) {
            continue;
        }
        kept.push(line);
    }
    kept.join("\n").trim_end().to_string()
}

/// The active prompt plus the built-in rules the user can't edit
/// ([`VOICE_CORE_RULES`], [`VOICE_TOPIC_RING_RULES`]) — what the voice
/// session actually runs with. Any older copy of those rules inside a
/// custom prompt is dropped first ([`strip_builtin_sections`]), so the
/// current version is the only one the session sees.
pub fn with_builtin_rules(content: &str) -> String {
    format!(
        "{}\n\n{VOICE_CORE_RULES}\n{VOICE_TOPIC_RING_RULES}",
        strip_builtin_sections(content)
    )
}

/// The system prompt to store on the voice session.
/// The system prompt to store on the voice session: [`with_builtin_rules`],
/// plus [`VOICE_AUTO_SHOW_RULES`] when the admin turned on
/// `auto_show_on_topic` (`service::assistant_settings`).
pub async fn session_prompt(db: &Db) -> anyhow::Result<String> {
    let auto_show = crate::service::assistant_settings::load(db)
        .await
        .auto_show_on_topic;
    Ok(with_auto_show(
        with_builtin_rules(&active_content(db).await?),
        auto_show,
    ))
}

/// `prompt` with the opt-in topic auto-show rules appended when `on`.
pub fn with_auto_show(prompt: String, on: bool) -> String {
    if on {
        format!("{prompt}\n{VOICE_AUTO_SHOW_RULES}")
    } else {
        prompt
    }
}

/// The stored prompt's Recent Memory snapshot: from its heading to the end
/// (`service::assistant_memory::render_block` always appends it last).
fn memory_snapshot(stored: &str) -> Option<&str> {
    let i = stored.find(&format!("\n{RECENT_MEMORY_HEADING}\n"))?;
    Some(&stored[i + 1..])
}

/// `prompt` with a Recent Memory snapshot appended, when there is one.
pub fn with_memory_snapshot(prompt: String, snapshot: Option<&str>) -> String {
    match snapshot {
        Some(s) => format!("{prompt}\n{s}"),
        None => prompt,
    }
}

/// Bring the voice session's stored prompt in line with the active one. With
/// a `registry`, a changed prompt also winds the session's live agent down
/// after its current turn so the next turn spawns with the new prompt.
/// Returns true when the session's prompt was rewritten.
///
/// The stored Recent Memory snapshot is carried over unchanged: memory
/// writes must not restart the agent, so the snapshot only moves at a
/// context clear or boot ([`refresh_memory_snapshot`]).
pub async fn apply_to_voice_session(
    db: &Db,
    registry: Option<&ProviderRegistry>,
) -> anyhow::Result<bool> {
    let Some(session) = db.find_expert_session(VOICE_EXPERT_KIND).await? else {
        return Ok(false);
    };
    let snapshot = session.system_prompt.as_deref().and_then(memory_snapshot);
    let content = with_memory_snapshot(session_prompt(db).await?, snapshot);
    store_if_changed(db, registry, &session, content).await
}

/// Rebuild the voice session's prompt with a fresh Recent Memory snapshot
/// (unexpired entries at `now`). Only the auto-clear and boot call this:
/// both happen with no agent mid-conversation. Returns true when rewritten.
pub async fn refresh_memory_snapshot(
    db: &Db,
    registry: Option<&ProviderRegistry>,
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<bool> {
    let Some(session) = db.find_expert_session(VOICE_EXPERT_KIND).await? else {
        return Ok(false);
    };
    let block = crate::service::assistant_memory::current_block(db, &session.id, now).await?;
    let content = with_memory_snapshot(session_prompt(db).await?, block.as_deref());
    store_if_changed(db, registry, &session, content).await
}

async fn store_if_changed(
    db: &Db,
    registry: Option<&ProviderRegistry>,
    session: &crate::db::models::Session,
    content: String,
) -> anyhow::Result<bool> {
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
        let expected = with_builtin_rules(&custom);
        assert_eq!(s.system_prompt.as_deref(), Some(expected.as_str()));

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
        let builtin = with_builtin_rules(VOICE_SYSTEM_PROMPT);
        assert_eq!(s.system_prompt.as_deref(), Some(builtin.as_str()));

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

    /// A prompt customised before the core rules were split out still gets
    /// them, exactly once.
    #[tokio::test]
    async fn custom_prompt_keeps_the_core_rules() {
        let db = voice_db().await;
        save(&db, "Be a pirate.", SOURCE_USER, None, None)
            .await
            .unwrap();
        assert!(apply_to_voice_session(&db, None).await.unwrap());
        let s = db.get_session("voice").await.unwrap().unwrap();
        let prompt = s.system_prompt.unwrap();
        assert!(prompt.starts_with("Be a pirate."));
        assert!(prompt.contains(VOICE_CORE_RULES));
        assert!(prompt.contains(VOICE_TOPIC_RING_RULES));
    }

    /// A custom prompt saved from an older default carries stale copies of
    /// the built-in sections (and the two built-in "How to Speak" bullets):
    /// they are dropped, the user's own text is kept, and every built-in
    /// section appears exactly once, in its current form.
    #[test]
    fn legacy_builtin_sections_are_not_duplicated() {
        let legacy = format!(
            "{VOICE_SYSTEM_PROMPT}\
             - When the user says \"pronounce X like Y\", call voice_pronunciation (old wording).\n\
             - When the user asks you to change how you behave, use voice_prompt (old wording).\n\
             - Keep it cheerful.\n\n\
             ## Routing Work to Sessions\n- old routing rule\n\
             ## Relay Messages\nold relay rule\n\n\
             ## My Household\n- The cat is called Miso.\n\n\
             ## The Topic Ring\nold ring rule\n"
        );
        let built = with_builtin_rules(&legacy);
        for gone in [
            "old routing rule",
            "old relay rule",
            "old ring rule",
            "(old wording)",
        ] {
            assert!(!built.contains(gone), "{gone} survived");
        }
        for kept in [
            "- Keep it cheerful.",
            "## My Household",
            "The cat is called Miso.",
        ] {
            assert!(built.contains(kept), "{kept} was dropped");
        }
        for heading in VOICE_CORE_RULES
            .lines()
            .chain(VOICE_TOPIC_RING_RULES.lines())
            .filter(|l| l.starts_with("## "))
        {
            assert_eq!(built.matches(heading).count(), 1, "{heading}");
        }
        assert_eq!(
            built
                .matches("call voice_pronunciation with action add")
                .count(),
            1
        );
        // Building from an already-built prompt changes nothing.
        assert_eq!(with_builtin_rules(&built), built);
        // The shipped default is untouched by the strip.
        assert_eq!(
            strip_builtin_sections(VOICE_SYSTEM_PROMPT),
            VOICE_SYSTEM_PROMPT.trim_end()
        );
    }

    /// Topic focus and the screen are decoupled by default: the session
    /// prompt carries the explicit-only show_view rule and no auto-show
    /// section until the admin turns `auto_show_on_topic` on.
    #[tokio::test]
    async fn auto_show_on_topic_is_off_by_default_and_opt_in() {
        use crate::service::assistant_settings::{self, AssistantSettings};
        let db = voice_db().await;
        let off = session_prompt(&db).await.unwrap();
        assert!(!off.contains(VOICE_AUTO_SHOW_RULES));
        assert!(off.contains("never calls show_view by itself"));
        assistant_settings::save(
            &db,
            &AssistantSettings {
                auto_show_on_topic: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let on = session_prompt(&db).await.unwrap();
        assert!(on.ends_with(VOICE_AUTO_SHOW_RULES));
        assert!(apply_to_voice_session(&db, None).await.unwrap());
        let s = db.get_session("voice").await.unwrap().unwrap();
        assert_eq!(s.system_prompt.as_deref(), Some(on.as_str()));
    }
}
