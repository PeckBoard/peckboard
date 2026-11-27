//! Periodic data-retention sweeper: bounds the growth of repeating-task run
//! sessions, session events, and report files per the admin-configured
//! [`crate::routes::settings::RetentionSettings`] (all-zero = keep forever).
//!
//! Mirrors the shape of `service::backup::spawn_scheduler` — an hourly
//! `tokio::time::interval` that skips its first tick (no sweep on boot) and
//! reloads settings every pass so a change takes effect without a restart.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use chrono::Utc;

use crate::routes::settings::{RetentionSettings, retention_settings};
use crate::state::AppState;

const SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

/// Spawn the hourly retention sweep loop.
pub fn spawn_sweeper(state: std::sync::Arc<AppState>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(SWEEP_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await; // skip the immediate first tick
        loop {
            interval.tick().await;
            run_sweep(&state).await;
        }
    });
    tracing::info!("Retention sweeper started (1h interval)");
}

/// Run one sweep pass over all three retention domains. Public so tests and
/// an eventual manual-trigger route can call it directly.
pub async fn run_sweep(state: &AppState) {
    let settings = retention_settings(state).await;
    sweep_repeating_sessions(state, &settings).await;
    sweep_events(state, &settings).await;
    if let Err(e) = sweep_reports(
        &state.config.data_dir,
        settings.report_max_age_days,
        settings.report_max_count,
    ) {
        tracing::warn!("Retention sweep: report prune failed: {e:#}");
    }
    // Short-term memory notes past their time. Readers already skip them;
    // this is what actually deletes the rows. Not an admin retention bound:
    // it always runs.
    match state.db.purge_expired_session_memories().await {
        Ok(0) => {}
        Ok(n) => tracing::info!("Retention sweep: deleted {n} expired short-term memory notes"),
        Err(e) => tracing::warn!("Retention sweep: memory purge failed: {e:#}"),
    }
    // Expired logins and tokens (they were only purged at boot or never),
    // then SQLite's own statistics upkeep.
    crate::service::db_maintenance::hourly_upkeep(&state.db).await;
    match crate::service::db_maintenance::prune_device_activity(
        &state.db,
        settings.device_activity_max_age_days,
    )
    .await
    {
        Ok(0) => {}
        Ok(n) => tracing::info!("Retention sweep: deleted {n} old device activity rows"),
        Err(e) => tracing::warn!("Retention sweep: device activity prune failed: {e:#}"),
    }
    // Files on disk that only ever grow: browser runs, tool images, shared
    // files, and provider CLI transcripts.
    let report = sweep_files(state, &settings, false).await;
    if report != FileSweepReport::default() {
        tracing::info!(
            browser_run_bytes = report.browser_runs.bytes,
            tool_image_bytes = report.tool_images.bytes,
            shared_file_bytes = report.shared_files.bytes,
            transcript_bytes = report.transcripts.bytes,
            sessions_reset = report.sessions_reset,
            "Retention sweep: pruned old files"
        );
    }
}

/// Delete repeating-task run sessions past the configured age and/or
/// per-task count bound. Deletion goes through `delete_session_core` so
/// events, attachments, MCP config/tokens, tabs, and todos cascade with it.
async fn sweep_repeating_sessions(state: &AppState, settings: &RetentionSettings) {
    if settings.repeating_session_max_age_days == 0 && settings.repeating_session_max_per_task == 0
    {
        return;
    }
    let rows = match state.db.list_repeating_run_sessions().await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("Retention sweep: listing repeating-task sessions failed: {e:#}");
            return;
        }
    };

    let age_cutoff = if settings.repeating_session_max_age_days > 0 {
        Some(
            (Utc::now() - chrono::Duration::days(settings.repeating_session_max_age_days as i64))
                .to_rfc3339(),
        )
    } else {
        None
    };

    let to_delete = select_repeating_sessions_to_prune(rows, settings, age_cutoff.as_deref());

    let mut deleted = 0usize;
    for id in to_delete {
        match crate::routes::sessions::delete_session_core(state, &id).await {
            Ok(_) => deleted += 1,
            Err(e) => {
                tracing::warn!(session_id = %id, "Retention sweep: session delete failed: {e:#}")
            }
        }
    }
    if deleted > 0 {
        tracing::info!(
            count = deleted,
            "Retention sweep: pruned repeating-task run sessions"
        );
    }
}

/// Pure selection logic for [`sweep_repeating_sessions`]: given `rows`
/// ordered `(repeating_task_id asc, last_activity desc)` — as
/// `Db::list_repeating_run_sessions` returns them — pick the session ids
/// past the per-task count bound and/or the age bound. A row fails the
/// count bound once its rank within its task exceeds
/// `max_per_task` (0 = no bound); it fails the age bound once its
/// `last_activity` sorts before `age_cutoff` (`None` = no bound).
fn select_repeating_sessions_to_prune(
    rows: Vec<(String, String, String)>,
    settings: &RetentionSettings,
    age_cutoff: Option<&str>,
) -> Vec<String> {
    let mut seen_for_task: HashMap<String, u32> = HashMap::new();
    let mut to_delete = Vec::new();
    for (id, task_id, last_activity) in rows {
        let rank = seen_for_task.entry(task_id).or_insert(0);
        *rank += 1;
        let over_count = settings.repeating_session_max_per_task > 0
            && *rank > settings.repeating_session_max_per_task;
        let over_age = age_cutoff.is_some_and(|cutoff| last_activity.as_str() < cutoff);
        if over_count || over_age {
            to_delete.push(id);
        }
    }
    to_delete
}
/// Delete events past the configured age (for idle/"terminal" non-worker
/// sessions) and/or per-session count bound.
async fn sweep_events(state: &AppState, settings: &RetentionSettings) {
    if settings.event_max_age_days > 0 {
        let cutoff_rfc3339 =
            (Utc::now() - chrono::Duration::days(settings.event_max_age_days as i64)).to_rfc3339();
        let cutoff_ms = (Utc::now() - chrono::Duration::days(settings.event_max_age_days as i64))
            .timestamp_millis();
        match state
            .db
            .list_idle_non_worker_session_ids(&cutoff_rfc3339)
            .await
        {
            Ok(ids) if !ids.is_empty() => {
                match state
                    .db
                    .delete_old_events_for_sessions(&ids, cutoff_ms)
                    .await
                {
                    Ok(n) if n > 0 => {
                        tracing::info!(count = n, "Retention sweep: pruned old events (age bound)")
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!("Retention sweep: event age-prune failed: {e:#}"),
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("Retention sweep: listing idle sessions failed: {e:#}"),
        }
    }

    if settings.event_max_count_per_session > 0 {
        let max_count = settings.event_max_count_per_session as i64;
        match state
            .db
            .list_non_worker_session_ids_over_event_count(max_count)
            .await
        {
            Ok(ids) => {
                let mut trimmed = 0usize;
                for id in ids {
                    match state.db.trim_events_to_count(&id, max_count).await {
                        Ok(n) => trimmed += n,
                        Err(e) => {
                            tracing::warn!(session_id = %id, "Retention sweep: event count-trim failed: {e:#}")
                        }
                    }
                }
                if trimmed > 0 {
                    tracing::info!(
                        count = trimmed,
                        "Retention sweep: pruned old events (count bound)"
                    );
                }
            }
            Err(e) => tracing::warn!("Retention sweep: listing over-count sessions failed: {e:#}"),
        }
    }
}

/// Delete report markdown files older than `max_age_days` and/or beyond the
/// newest `max_count` overall (by mtime). Either bound 0 disables it.
/// Returns the number of files removed.
pub fn sweep_reports(data_dir: &Path, max_age_days: u32, max_count: u32) -> anyhow::Result<usize> {
    if max_age_days == 0 && max_count == 0 {
        return Ok(0);
    }
    let reports_dir = data_dir.join("reports");
    if !reports_dir.exists() {
        return Ok(0);
    }

    let mut entries: Vec<(std::path::PathBuf, SystemTime)> = Vec::new();
    for folder in std::fs::read_dir(&reports_dir)?.flatten() {
        let folder_path = folder.path();
        if !folder_path.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(&folder_path)?.flatten() {
            let path = file.path();
            if path.extension().is_some_and(|e| e == "md")
                && let Ok(meta) = file.metadata()
                && let Ok(modified) = meta.modified()
            {
                entries.push((path, modified));
            }
        }
    }

    let mut deleted = 0usize;
    let now = SystemTime::now();

    if max_age_days > 0 {
        let max_age = Duration::from_secs(max_age_days as u64 * 86_400);
        entries.retain(|(path, modified)| {
            if now.duration_since(*modified).unwrap_or_default() > max_age {
                if std::fs::remove_file(path).is_ok() {
                    deleted += 1;
                }
                false
            } else {
                true
            }
        });
    }

    if max_count > 0 && entries.len() > max_count as usize {
        entries.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified)); // newest first
        for (path, _) in entries.split_off(max_count as usize) {
            if std::fs::remove_file(&path).is_ok() {
                deleted += 1;
            }
        }
    }

    Ok(deleted)
}

// ── Files that only grow: browser runs, tool images, CLI transcripts ────

/// What one file prune removed (or, on a dry run, would remove).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct PruneStats {
    pub files: u64,
    pub bytes: u64,
}

/// Result of [`sweep_files`], also the body of the Settings preview.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct FileSweepReport {
    pub browser_runs: PruneStats,
    pub tool_images: PruneStats,
    pub shared_files: PruneStats,
    pub transcripts: PruneStats,
    /// Sessions idle past the transcript limit whose conversation pointer
    /// was (or would be) dropped, so their next turn starts fresh.
    pub sessions_reset: u64,
}

/// The only places a transcript is deleted from: per provider, the folder
/// under the data dir holding one directory per Peckboard-managed account,
/// and the subfolders of an account that hold conversations. Everything
/// else in an account folder (sign-in files, settings, caches, the CLI's
/// own databases) is never touched, and neither is the host's own
/// `~/.claude` / `~/.codex` / `~/.grok`.
const TRANSCRIPT_DIRS: &[(&str, &[&str])] = &[
    ("claude-accounts", &["projects"]),
    ("codex-accounts", &["sessions", "generated_images"]),
    ("grok-accounts", &["sessions"]),
];

fn days_ago(days: u32) -> SystemTime {
    SystemTime::now()
        .checked_sub(Duration::from_secs(days as u64 * 86_400))
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Run (or, with `dry_run`, only measure) the three file prunes for the
/// limits in `settings`; a limit of 0 skips that kind.
pub async fn sweep_files(
    state: &AppState,
    settings: &RetentionSettings,
    dry_run: bool,
) -> FileSweepReport {
    let mut report = FileSweepReport::default();
    let data_dir = state.config.data_dir.clone();

    let (run_days, image_days, shared_days) = (
        settings.browser_run_max_age_days,
        settings.tool_image_max_age_days,
        settings.shared_file_max_age_days,
    );
    if run_days > 0 || image_days > 0 || shared_days > 0 {
        let dir = data_dir.clone();
        let done = tokio::task::spawn_blocking(move || {
            let runs = if run_days > 0 {
                sweep_browser_runs(&dir, days_ago(run_days), dry_run)
            } else {
                PruneStats::default()
            };
            // Both are one folder per session holding plain files, and a
            // chat that still shows one says it is gone (the routes 404).
            let by_age = |name: &str, days: u32| {
                let mut stats = PruneStats::default();
                if days > 0 {
                    prune_tree(
                        &dir.join(name),
                        days_ago(days),
                        &|_| false,
                        dry_run,
                        &mut stats,
                    );
                }
                stats
            };
            (
                runs,
                by_age("tool-images", image_days),
                by_age("shared-files", shared_days),
            )
        })
        .await;
        if let Ok((runs, images, shared)) = done {
            report.browser_runs = runs;
            report.tool_images = images;
            report.shared_files = shared;
        }
    }

    let days = settings.transcript_max_age_days;
    if days > 0 {
        let rows = match state.db.list_session_conversations().await {
            Ok(rows) => rows,
            Err(e) => {
                // Without the keep-list nothing may be deleted.
                tracing::warn!("Retention sweep: listing session conversations failed: {e:#}");
                return report;
            }
        };
        let cutoff = (Utc::now() - chrono::Duration::days(days as i64)).to_rfc3339();
        let (idle, mut keep) = split_conversations(rows, &cutoff);
        // Sessions first: a session still pointing at a deleted transcript
        // would fail its next turn before recovering. One that can't be
        // reset keeps its files.
        for (session_id, conversation_id) in idle {
            if dry_run || reset_idle_session(state, &session_id, days).await {
                report.sessions_reset += 1;
            } else {
                keep.insert(conversation_id);
            }
        }
        let dir = data_dir.clone();
        let done = tokio::task::spawn_blocking(move || {
            sweep_transcript_files(&dir, days_ago(days), &keep, dry_run)
        })
        .await;
        if let Ok(stats) = done {
            report.transcripts = stats;
        }
    }
    report
}

/// Conversation ids whose files must survive a transcript sweep.
#[derive(Debug, Default)]
struct KeepIds {
    /// The usual shape: a 36-character uuid, found by window lookup.
    uuids: std::collections::HashSet<String>,
    /// Anything else, matched by substring.
    other: Vec<String>,
}

impl KeepIds {
    fn insert(&mut self, id: String) {
        if id.len() == 36 {
            self.uuids.insert(id.to_ascii_lowercase());
        } else if !id.trim().is_empty() {
            self.other.push(id);
        }
    }

    /// True when `path` names a kept conversation anywhere in it — the
    /// transcript file itself, or a folder of its images / subagent logs.
    fn mentions(&self, path: &Path) -> bool {
        let text = path.to_string_lossy().to_ascii_lowercase();
        if self.other.iter().any(|id| text.contains(id.as_str())) {
            return true;
        }
        if self.uuids.is_empty() {
            return false;
        }
        text.split(|c: char| !(c.is_ascii_hexdigit() || c == '-'))
            .filter(|run| run.len() >= 36)
            .any(|run| (0..=run.len() - 36).any(|i| self.uuids.contains(&run[i..i + 36])))
    }
}

/// Split `(session id, conversation id, last_activity)` rows at `cutoff`
/// (RFC 3339, compared as text like the other sweeps): sessions idle since
/// before it are returned for a reset, every other session's conversation
/// goes on the keep-list.
fn split_conversations(
    rows: Vec<(String, String, String)>,
    cutoff: &str,
) -> (Vec<(String, String)>, KeepIds) {
    let mut idle = Vec::new();
    let mut keep = KeepIds::default();
    for (session_id, conversation_id, last_activity) in rows {
        if last_activity.as_str() < cutoff {
            idle.push((session_id, conversation_id));
        } else {
            keep.insert(conversation_id);
        }
    }
    (idle, keep)
}

/// Drop an idle session's conversation pointer and leave the marker that
/// stops the event-log fallback from handing the same id back (see
/// `provider::manager::resume_conversation_id_from_tail`). Its chat history
/// stays; its next turn starts cold. False when the pointer could not be
/// cleared, in which case the caller keeps the transcript.
async fn reset_idle_session(state: &AppState, session_id: &str, days: u32) -> bool {
    let update = crate::db::models::UpdateSession {
        conversation_id: Some(None),
        ..Default::default()
    };
    match state.db.update_session(session_id, update).await {
        Ok(Some(_)) => {}
        Ok(None) => return false,
        Err(e) => {
            tracing::warn!(session_id = %session_id, "Retention sweep: session reset failed: {e:#}");
            return false;
        }
    }
    if let Err(e) = state
        .db
        .append_event(
            session_id,
            crate::provider::resume_recovery::CONVERSATION_RESET_KIND,
            serde_json::json!({ "reason": "transcript-pruned", "days": days }),
        )
        .await
    {
        tracing::warn!(session_id = %session_id, "Retention sweep: reset marker failed: {e:#}");
    }
    true
}

/// Delete whole browser-run folders in which nothing was written since
/// `cutoff`. A run is one unit (frames + DOM log + meta), so it goes or
/// stays as a whole; a run still recording keeps writing and is skipped.
fn sweep_browser_runs(data_dir: &Path, cutoff: SystemTime, dry_run: bool) -> PruneStats {
    let mut stats = PruneStats::default();
    let root = crate::service::browser_runs::runs_root(data_dir);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return stats;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let mut run = PruneStats::default();
        // Age is the newest FILE: a folder's own time moves whenever an
        // entry is added or removed. An empty run falls back to the folder.
        let mut newest = None;
        measure_tree(&path, &mut run, &mut newest);
        let newest = newest.or_else(|| entry.metadata().ok().and_then(|m| m.modified().ok()));
        if newest.is_some_and(|t| t < cutoff) && (dry_run || std::fs::remove_dir_all(&path).is_ok())
        {
            stats.files += run.files;
            stats.bytes += run.bytes;
        }
    }
    stats
}

/// Total file count / bytes under `dir` and the newest modification time.
fn measure_tree(dir: &Path, stats: &mut PruneStats, newest: &mut Option<SystemTime>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if meta.is_dir() {
            measure_tree(&entry.path(), stats, newest);
        } else {
            stats.files += 1;
            stats.bytes += meta.len();
            if let Ok(modified) = meta.modified()
                && newest.is_none_or(|n| modified > n)
            {
                *newest = Some(modified);
            }
        }
    }
}

/// Delete transcripts older than `cutoff` from every managed account's
/// conversation folders ([`TRANSCRIPT_DIRS`]), except files naming a kept
/// conversation.
fn sweep_transcript_files(
    data_dir: &Path,
    cutoff: SystemTime,
    keep: &KeepIds,
    dry_run: bool,
) -> PruneStats {
    let mut stats = PruneStats::default();
    for (accounts, subdirs) in TRANSCRIPT_DIRS {
        let Ok(entries) = std::fs::read_dir(data_dir.join(accounts)) else {
            continue;
        };
        for account in entries.flatten() {
            if !account.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            for sub in *subdirs {
                prune_tree(
                    &account.path().join(sub),
                    cutoff,
                    &|p| keep.mentions(p),
                    dry_run,
                    &mut stats,
                );
            }
        }
    }
    stats
}

/// Delete every regular file under `root` last modified before `cutoff`
/// unless `keep` claims it, then remove folders left empty (never `root`
/// itself). Symlinks are neither followed nor removed. Returns whether
/// `root` ended up empty.
fn prune_tree(
    root: &Path,
    cutoff: SystemTime,
    keep: &dyn Fn(&Path) -> bool,
    dry_run: bool,
    stats: &mut PruneStats,
) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    let mut empty = true;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            empty = false;
            continue;
        };
        if meta.is_dir() {
            let now_empty = prune_tree(&path, cutoff, keep, dry_run, stats);
            if !(now_empty && (dry_run || std::fs::remove_dir(&path).is_ok())) {
                empty = false;
            }
        } else if meta.is_file()
            && meta.modified().is_ok_and(|m| m < cutoff)
            && !keep(&path)
            && (dry_run || std::fs::remove_file(&path).is_ok())
        {
            stats.files += 1;
            stats.bytes += meta.len();
        } else {
            empty = false;
        }
    }
    empty
}

/// Disk used right now by each kind of file the sweep can prune, plus the
/// database for scale. The body of the Settings size readout.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct UsageReport {
    pub browser_runs: PruneStats,
    pub tool_images: PruneStats,
    pub shared_files: PruneStats,
    pub transcripts: PruneStats,
    /// `peckboard.db` and its write-ahead files. Never pruned by age.
    pub database: PruneStats,
}

/// Measure [`UsageReport`] by walking the data dir. Read-only.
pub async fn measure_usage(data_dir: &Path) -> UsageReport {
    let dir = data_dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let tree = |path: std::path::PathBuf| {
            let mut stats = PruneStats::default();
            measure_tree(&path, &mut stats, &mut None);
            stats
        };
        let mut transcripts = PruneStats::default();
        for (accounts, subdirs) in TRANSCRIPT_DIRS {
            let Ok(entries) = std::fs::read_dir(dir.join(accounts)) else {
                continue;
            };
            for account in entries.flatten() {
                for sub in *subdirs {
                    measure_tree(&account.path().join(sub), &mut transcripts, &mut None);
                }
            }
        }
        let mut database = PruneStats::default();
        for name in ["peckboard.db", "peckboard.db-wal", "peckboard.db-shm"] {
            if let Ok(meta) = std::fs::metadata(dir.join(name)) {
                database.files += 1;
                database.bytes += meta.len();
            }
        }
        UsageReport {
            browser_runs: tree(crate::service::browser_runs::runs_root(&dir)),
            tool_images: tree(dir.join("tool-images")),
            shared_files: tree(dir.join("shared-files")),
            transcripts,
            database,
        }
    })
    .await
    .unwrap_or_default()
}
#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, task: &str, last_activity: &str) -> (String, String, String) {
        (id.to_string(), task.to_string(), last_activity.to_string())
    }

    #[test]
    fn prune_by_count_keeps_newest_n_per_task() {
        let rows = vec![
            row("t1-new", "t1", "2026-06-01T00:00:00+00:00"),
            row("t1-mid", "t1", "2026-05-01T00:00:00+00:00"),
            row("t1-old", "t1", "2026-04-01T00:00:00+00:00"),
            row("t2-only", "t2", "2026-01-01T00:00:00+00:00"),
        ];
        let settings = RetentionSettings {
            repeating_session_max_per_task: 2,
            ..Default::default()
        };
        let pruned = select_repeating_sessions_to_prune(rows, &settings, None);
        assert_eq!(pruned, vec!["t1-old".to_string()]);
    }

    #[test]
    fn prune_by_age_ignores_count() {
        let rows = vec![
            row("new", "t1", "2026-06-01T00:00:00+00:00"),
            row("old", "t1", "2026-01-01T00:00:00+00:00"),
        ];
        let settings = RetentionSettings::default();
        let pruned =
            select_repeating_sessions_to_prune(rows, &settings, Some("2026-03-01T00:00:00+00:00"));
        assert_eq!(pruned, vec!["old".to_string()]);
    }

    #[test]
    fn prune_disabled_when_both_bounds_zero() {
        let rows = vec![row("a", "t1", "2020-01-01T00:00:00+00:00")];
        let settings = RetentionSettings::default();
        assert!(select_repeating_sessions_to_prune(rows, &settings, None).is_empty());
    }

    fn touch(dir: &Path, folder: &str, file: &str) -> std::path::PathBuf {
        let folder_path = dir.join(folder);
        std::fs::create_dir_all(&folder_path).unwrap();
        let path = folder_path.join(file);
        std::fs::write(&path, "---\ntitle: x\n---\nbody").unwrap();
        path
    }

    #[test]
    fn sweep_reports_respects_max_count() {
        let tmp = tempfile::tempdir().unwrap();
        let reports_dir = tmp.path().join("reports");
        touch(&reports_dir, "2026-01-01", "a.md");
        touch(&reports_dir, "2026-01-01", "b.md");
        touch(&reports_dir, "2026-01-01", "c.md");

        let deleted = sweep_reports(tmp.path(), 0, 2).unwrap();
        assert_eq!(deleted, 1);
        let remaining = std::fs::read_dir(reports_dir.join("2026-01-01"))
            .unwrap()
            .count();
        assert_eq!(remaining, 2);
    }

    #[test]
    fn sweep_reports_noop_when_both_bounds_zero() {
        let tmp = tempfile::tempdir().unwrap();
        touch(&tmp.path().join("reports"), "2026-01-01", "a.md");
        assert_eq!(sweep_reports(tmp.path(), 0, 0).unwrap(), 0);
    }

    #[test]
    fn sweep_reports_missing_dir_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(sweep_reports(tmp.path(), 30, 10).unwrap(), 0);
    }

    /// Write `rel` under `root` and backdate it by `age_days`.
    fn aged(root: &Path, rel: &str, age_days: u64) -> std::path::PathBuf {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"0123456789").unwrap();
        let when = SystemTime::now() - Duration::from_secs(age_days * 86_400);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(when)
            .unwrap();
        path
    }

    #[test]
    fn browser_runs_go_whole_and_only_when_nothing_in_them_is_recent() {
        let tmp = tempfile::tempdir().unwrap();
        let old = aged(tmp.path(), "browser-runs/old/0000.png", 40);
        aged(tmp.path(), "browser-runs/old/meta.json", 40);
        // One fresh frame keeps the whole run, old frames included.
        let mixed = aged(tmp.path(), "browser-runs/live/0000.png", 40);
        aged(tmp.path(), "browser-runs/live/0001.png", 0);

        let dry = sweep_browser_runs(tmp.path(), days_ago(30), true);
        assert_eq!(
            dry,
            PruneStats {
                files: 2,
                bytes: 20
            }
        );
        assert!(old.exists(), "a dry run deletes nothing");

        let done = sweep_browser_runs(tmp.path(), days_ago(30), false);
        assert_eq!(done, dry);
        assert!(!old.parent().unwrap().exists());
        assert!(mixed.exists());
    }

    #[test]
    fn transcript_sweep_spares_kept_conversations_and_everything_outside_the_allowlist() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let kept = "11111111-2222-3333-4444-555555555555";
        let gone = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let auth = aged(d, "codex-accounts/c1/auth.json", 400);
        let history = aged(d, "codex-accounts/c1/thread_history_1.sqlite", 400);
        let claude_keys = aged(d, "claude-accounts/a1/sessions/1.key", 400);
        let old = aged(
            d,
            &format!(
                "codex-accounts/c1/sessions/2026/01/02/rollout-2026-01-02T10-00-00-{gone}.jsonl"
            ),
            90,
        );
        let old_kept = aged(
            d,
            &format!(
                "codex-accounts/c1/sessions/2026/01/02/rollout-2026-01-02T11-00-00-{kept}.jsonl"
            ),
            90,
        );
        let kept_image = aged(
            d,
            &format!("codex-accounts/c1/generated_images/{kept}/1.png"),
            90,
        );
        let old_claude = aged(
            d,
            &format!("claude-accounts/a1/projects/-home-x/{gone}.jsonl"),
            90,
        );
        let old_sub = aged(
            d,
            &format!("claude-accounts/a1/projects/-home-x/{gone}/subagents/agent-1.jsonl"),
            90,
        );
        let fresh = aged(d, "grok-accounts/g1/sessions/%2Fhome%2Fx/s1/chat.jsonl", 3);
        let old_grok = aged(d, "grok-accounts/g1/sessions/%2Fhome%2Fx/s0/chat.jsonl", 90);

        let (idle, keep) = split_conversations(
            vec![
                row("s-active", kept, "2026-06-01T00:00:00+00:00"),
                row("s-idle", gone, "2026-01-01T00:00:00+00:00"),
            ],
            "2026-03-01T00:00:00+00:00",
        );
        assert_eq!(idle, vec![("s-idle".to_string(), gone.to_string())]);

        let stats = sweep_transcript_files(d, days_ago(60), &keep, false);
        assert_eq!(
            stats,
            PruneStats {
                files: 4,
                bytes: 40
            }
        );
        for gone in [&old, &old_claude, &old_sub, &old_grok] {
            assert!(!gone.exists(), "{} should be deleted", gone.display());
        }
        assert!(
            !old_sub.parent().unwrap().parent().unwrap().exists(),
            "emptied folders go"
        );
        for stays in [
            &auth,
            &history,
            &claude_keys,
            &old_kept,
            &kept_image,
            &fresh,
        ] {
            assert!(stays.exists(), "{} must survive", stays.display());
        }
    }

    #[tokio::test]
    async fn file_sweep_resets_idle_sessions_before_deleting_and_leaves_active_ones() {
        use crate::db::models::{NewFolder, NewSession};
        let tmp = tempfile::tempdir().unwrap();
        let state = crate::auth::middleware::tests::test_state(tmp.path());
        let d = state.config.data_dir.clone();
        let now = Utc::now();
        state
            .db
            .create_folder(NewFolder {
                id: "f".into(),
                name: "f".into(),
                path: ".".into(),
                created_at: now.to_rfc3339(),
            })
            .await
            .unwrap();
        let idle_conv = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let live_conv = "11111111-2222-3333-4444-555555555555";
        for (id, conv, age_days) in [("idle", idle_conv, 90), ("live", live_conv, 1)] {
            let at = (now - chrono::Duration::days(age_days)).to_rfc3339();
            state
                .db
                .create_session(NewSession {
                    id: id.into(),
                    name: id.into(),
                    folder_id: "f".into(),
                    conversation_id: Some(conv.into()),
                    created_at: at.clone(),
                    last_activity: at,
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        let idle_file = aged(
            &d,
            &format!("claude-accounts/a/projects/p/{idle_conv}.jsonl"),
            90,
        );
        let live_file = aged(
            &d,
            &format!("claude-accounts/a/projects/p/{live_conv}.jsonl"),
            90,
        );
        let image = aged(&d, "tool-images/idle/img", 90);

        // All limits off: nothing happens.
        let off = sweep_files(&state, &RetentionSettings::default(), false).await;
        assert_eq!(off, FileSweepReport::default());

        let settings = RetentionSettings {
            tool_image_max_age_days: 30,
            transcript_max_age_days: 60,
            ..Default::default()
        };
        let preview = sweep_files(&state, &settings, true).await;
        assert_eq!(preview.sessions_reset, 1);
        assert_eq!(preview.transcripts.files, 1);
        assert!(
            idle_file.exists() && image.exists(),
            "a preview deletes nothing"
        );
        let idle = state.db.get_session("idle").await.unwrap().unwrap();
        assert_eq!(idle.conversation_id.as_deref(), Some(idle_conv));

        let done = sweep_files(&state, &settings, false).await;
        assert_eq!(done, preview);
        assert!(!idle_file.exists() && !image.exists());
        assert!(live_file.exists(), "an active session keeps its transcript");
        let idle = state.db.get_session("idle").await.unwrap().unwrap();
        assert_eq!(idle.conversation_id, None);
        let live = state.db.get_session("live").await.unwrap().unwrap();
        assert_eq!(live.conversation_id.as_deref(), Some(live_conv));
        // The marker stops the event-log fallback re-serving the dead id.
        let events = state.db.events_tail("idle", 5).await.unwrap();
        assert!(events.iter().any(|e| e.kind == "conversation-reset"));
    }

    #[tokio::test]
    async fn shared_files_are_measured_and_pruned_by_age() {
        let tmp = tempfile::tempdir().unwrap();
        let state = crate::auth::middleware::tests::test_state(tmp.path());
        let d = state.config.data_dir.clone();
        let old = aged(&d, "shared-files/s1/old", 40);
        let old_meta = aged(&d, "shared-files/s1/old.meta", 40);
        let fresh = aged(&d, "shared-files/s2/fresh", 1);

        let usage = measure_usage(&d).await;
        assert_eq!(usage.shared_files.files, 3);
        assert_eq!(usage.tool_images, PruneStats::default());

        let settings = RetentionSettings {
            shared_file_max_age_days: 30,
            ..Default::default()
        };
        let preview = sweep_files(&state, &settings, true).await;
        assert_eq!(preview.shared_files.files, 2);
        assert!(old.exists(), "a preview deletes nothing");

        let done = sweep_files(&state, &settings, false).await;
        assert_eq!(done, preview);
        assert!(!old.exists() && !old_meta.exists());
        assert!(fresh.exists(), "a recent shared file stays");
        assert!(
            !d.join("shared-files/s1").exists(),
            "the emptied folder goes"
        );
    }
}
