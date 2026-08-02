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
}
