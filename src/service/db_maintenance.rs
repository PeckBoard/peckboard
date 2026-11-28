//! Database upkeep behind Settings → Data → Database: a size readout, a
//! cleanup of rows nothing points at anymore, compaction, and an integrity
//! check.
//!
//! The cleanup is a fixed list of [`Rule`]s. A rule is one table plus one SQL
//! condition, and both the preview (`SELECT COUNT(*)`) and the delete are
//! built from that same pair, so a preview cannot name rows the cleanup then
//! leaves, or the other way round. Rules marked `hourly` (expired logins and
//! tokens) also run from the retention sweeper.
//!
//! Never removed here: usage / billing history (`usage_events` outlives the
//! events it was read from, by design), card run history (`card_sessions`),
//! and the `pending_actions` audit log.

use std::path::Path;

use diesel::prelude::*;
use diesel::sql_query;
use diesel::sql_types::{BigInt, Text};
use diesel::sqlite::SqliteConnection;

use crate::db::Db;
use crate::state::AppState;

/// What a rule's `?1` placeholder is bound to.
#[derive(Clone, Copy)]
enum Bind {
    None,
    /// Unix seconds, the unit of `auth_sessions` / `mfa_*` expiry columns.
    NowSecs,
    /// `oauth_access_tokens.expires_at` text form.
    NowOauth,
}

struct Rule {
    key: &'static str,
    /// Shown in Settings; plain words for someone who is not a developer.
    label: &'static str,
    table: &'static str,
    condition: &'static str,
    bind: Bind,
    /// Also run by the hourly retention sweep.
    hourly: bool,
}

const SESSION_GONE: &str = "session_id NOT IN (SELECT id FROM sessions)";

const RULES: &[Rule] = &[
    Rule {
        key: "events",
        label: "Chat history of deleted sessions",
        table: "events",
        condition: SESSION_GONE,
        bind: Bind::None,
        hourly: false,
    },
    Rule {
        key: "queued_messages",
        label: "Queued messages of deleted sessions",
        table: "queued_messages",
        condition: SESSION_GONE,
        bind: Bind::None,
        hourly: false,
    },
    Rule {
        key: "todos",
        label: "To-do lists of deleted sessions",
        table: "todos",
        condition: SESSION_GONE,
        bind: Bind::None,
        hourly: false,
    },
    Rule {
        key: "plugin_session_meta",
        label: "Plugin data of deleted sessions",
        table: "plugin_session_meta",
        condition: SESSION_GONE,
        bind: Bind::None,
        hourly: false,
    },
    // A plan is reachable from its session or from its card; it is garbage
    // only once both are gone.
    Rule {
        key: "plans",
        label: "Plans of deleted sessions",
        table: "plans",
        condition: "session_id NOT IN (SELECT id FROM sessions) \
                    AND (card_id IS NULL OR card_id NOT IN (SELECT id FROM cards))",
        bind: Bind::None,
        hourly: false,
    },
    Rule {
        key: "voice_topics",
        label: "Conversation topics of deleted assistants",
        table: "voice_topics",
        condition: SESSION_GONE,
        bind: Bind::None,
        hourly: false,
    },
    // `user_tabs` is polymorphic with no foreign key. `report` tabs point at
    // files, not rows, and are left alone.
    Rule {
        key: "user_tabs",
        label: "Open tabs for things that were deleted",
        table: "user_tabs",
        condition: "(item_type = 'session' AND item_id NOT IN (SELECT id FROM sessions)) \
                    OR (item_type = 'project' AND item_id NOT IN (SELECT id FROM projects)) \
                    OR (item_type = 'repeating_task' AND item_id NOT IN (SELECT id FROM repeating_tasks)) \
                    OR (item_type = 'doc_review' AND item_id NOT IN (SELECT id FROM doc_reviews)) \
                    OR (item_type = 'terminal' AND item_id NOT IN (SELECT id FROM terminals))",
        bind: Bind::None,
        hourly: false,
    },
    Rule {
        key: "auth_sessions",
        label: "Expired logins",
        table: "auth_sessions",
        condition: "expires_at < ?1",
        bind: Bind::NowSecs,
        hourly: true,
    },
    Rule {
        key: "mfa_challenges",
        label: "Expired two-step sign-in attempts",
        table: "mfa_challenges",
        condition: "expires_at < ?1",
        bind: Bind::NowSecs,
        hourly: true,
    },
    Rule {
        key: "mfa_pending",
        label: "Expired two-step setup attempts",
        table: "mfa_pending",
        condition: "expires_at < ?1",
        bind: Bind::NowSecs,
        hourly: true,
    },
    Rule {
        key: "oauth_access_tokens",
        label: "Expired access tokens of connected apps",
        table: "oauth_access_tokens",
        condition: "expires_at <= ?1",
        bind: Bind::NowOauth,
        hourly: true,
    },
];

/// A temp session with no tab is only an orphan once its creator has had
/// time to open the tab: the client creates the session first.
const TEMP_SESSION_GRACE_MINUTES: i64 = 60;
const TEMP_SESSIONS_KEY: &str = "temp_sessions";
const TEMP_SESSIONS_LABEL: &str = "Abandoned temporary sessions";

#[derive(QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

#[derive(QueryableByName)]
struct NameRow {
    #[diesel(sql_type = Text)]
    name: String,
}

/// One line of a cleanup preview or result.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CleanupItem {
    pub key: &'static str,
    pub label: &'static str,
    pub rows: i64,
}

/// What a cleanup would remove (preview) or did remove (run). Every rule is
/// listed, including those with nothing to remove.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CleanupReport {
    pub items: Vec<CleanupItem>,
    pub total: i64,
}

impl CleanupReport {
    fn new(items: Vec<CleanupItem>) -> Self {
        let total = items.iter().map(|i| i.rows).sum();
        Self { items, total }
    }
}

/// Count (`delete == false`) or delete the rows one rule matches.
fn apply_rule(conn: &mut SqliteConnection, rule: &Rule, delete: bool) -> anyhow::Result<i64> {
    let from = format!("FROM {} WHERE {}", rule.table, rule.condition);
    let now = chrono::Utc::now();
    if delete {
        let q = sql_query(format!("DELETE {from}"));
        let n = match rule.bind {
            Bind::None => q.execute(conn)?,
            Bind::NowSecs => q.bind::<BigInt, _>(now.timestamp()).execute(conn)?,
            Bind::NowOauth => q
                .bind::<Text, _>(crate::db::crud::oauth_ts(now))
                .execute(conn)?,
        };
        Ok(n as i64)
    } else {
        let q = sql_query(format!("SELECT COUNT(*) AS n {from}"));
        let row: CountRow = match rule.bind {
            Bind::None => q.get_result(conn)?,
            Bind::NowSecs => q.bind::<BigInt, _>(now.timestamp()).get_result(conn)?,
            Bind::NowOauth => q
                .bind::<Text, _>(crate::db::crud::oauth_ts(now))
                .get_result(conn)?,
        };
        Ok(row.n)
    }
}

fn apply_rules(
    conn: &mut SqliteConnection,
    delete: bool,
    only_hourly: bool,
) -> anyhow::Result<Vec<CleanupItem>> {
    RULES
        .iter()
        .filter(|r| !only_hourly || r.hourly)
        .map(|rule| {
            Ok(CleanupItem {
                key: rule.key,
                label: rule.label,
                rows: apply_rule(conn, rule, delete)?,
            })
        })
        .collect()
}

/// Temp sessions no tab points at, older than the grace period. Same rule as
/// the startup sweep (`list_orphan_temp_session_ids`) plus the age guard a
/// sweep on a running server needs.
async fn abandoned_temp_sessions(db: &Db) -> anyhow::Result<Vec<String>> {
    let cutoff =
        (chrono::Utc::now() - chrono::Duration::minutes(TEMP_SESSION_GRACE_MINUTES)).to_rfc3339();
    db.with_conn(move |conn| {
        #[derive(QueryableByName)]
        struct IdRow {
            #[diesel(sql_type = Text)]
            id: String,
        }
        let rows: Vec<IdRow> = sql_query(
            "SELECT id FROM sessions \
             WHERE is_temp = 1 AND is_worker = 0 AND created_at < ?1 \
               AND NOT EXISTS (SELECT 1 FROM user_tabs \
                               WHERE item_type = 'session' AND item_id = sessions.id)",
        )
        .bind::<Text, _>(cutoff)
        .load(conn)?;
        Ok(rows.into_iter().map(|r| r.id).collect())
    })
    .await
}

/// What [`run_cleanup`] would remove right now. Deletes nothing.
pub async fn preview_cleanup(state: &AppState) -> anyhow::Result<CleanupReport> {
    let temp = abandoned_temp_sessions(&state.db).await?.len() as i64;
    let mut items = state
        .db
        .with_conn(|conn| apply_rules(conn, false, false))
        .await?;
    items.push(CleanupItem {
        key: TEMP_SESSIONS_KEY,
        label: TEMP_SESSIONS_LABEL,
        rows: temp,
    });
    Ok(CleanupReport::new(items))
}

/// Remove everything the rules match and report what went. Temp sessions go
/// first, through the full session delete, so whatever they leave behind is
/// caught by the rules in the same pass. The rules run in one transaction.
pub async fn run_cleanup(state: &AppState) -> anyhow::Result<CleanupReport> {
    let mut temp = 0i64;
    for id in abandoned_temp_sessions(&state.db).await? {
        match crate::routes::sessions::delete_session_core(state, &id).await {
            Ok(true) => temp += 1,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(session_id = %id, "Database cleanup: temp session delete failed: {e:#}")
            }
        }
    }
    let mut items = state
        .db
        .with_conn(|conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| apply_rules(conn, true, false))
        })
        .await?;
    items.push(CleanupItem {
        key: TEMP_SESSIONS_KEY,
        label: TEMP_SESSIONS_LABEL,
        rows: temp,
    });
    let report = CleanupReport::new(items);
    tracing::info!(rows = report.total, "Database cleanup finished");
    Ok(report)
}

/// The hourly part: drop expired logins and tokens, then let SQLite refresh
/// the planner statistics it thinks are stale (`PRAGMA optimize` is cheap
/// and a no-op when nothing changed much).
pub async fn hourly_upkeep(db: &Db) {
    let res = db
        .with_conn(|conn| {
            let items = apply_rules(conn, true, true)?;
            sql_query("PRAGMA optimize").execute(conn).ok();
            Ok(items)
        })
        .await;
    match res {
        Ok(items) => {
            for item in items.iter().filter(|i| i.rows > 0) {
                tracing::info!(
                    "Retention sweep: deleted {} rows ({})",
                    item.rows,
                    item.label
                );
            }
        }
        Err(e) => tracing::warn!("Retention sweep: expired-row purge failed: {e:#}"),
    }
}

/// Delete `device_activity` rows older than `max_age_days` (0 = keep all).
pub async fn prune_device_activity(db: &Db, max_age_days: u32) -> anyhow::Result<usize> {
    if max_age_days == 0 {
        return Ok(0);
    }
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(max_age_days as i64)).to_rfc3339();
    db.with_conn(move |conn| {
        sql_query("DELETE FROM device_activity WHERE created_at < ?1")
            .bind::<Text, _>(cutoff)
            .execute(conn)
            .map_err(Into::into)
    })
    .await
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TableRows {
    pub name: String,
    pub rows: i64,
}

/// Size readout for the Settings section.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DbStats {
    /// `peckboard.db` plus its write-ahead files.
    pub file_bytes: u64,
    /// Free pages inside the file: what "Compact database" gives back.
    pub reclaimable_bytes: u64,
    /// Every table, largest row count first.
    pub tables: Vec<TableRows>,
}

fn file_bytes(data_dir: &Path) -> u64 {
    ["peckboard.db", "peckboard.db-wal", "peckboard.db-shm"]
        .iter()
        .filter_map(|name| std::fs::metadata(data_dir.join(name)).ok())
        .map(|m| m.len())
        .sum()
}

fn pragma_i64(conn: &mut SqliteConnection, pragma: &str) -> anyhow::Result<i64> {
    let row: CountRow =
        sql_query(format!("SELECT {pragma} AS n FROM pragma_{pragma}()")).get_result(conn)?;
    Ok(row.n)
}

pub async fn stats(state: &AppState) -> anyhow::Result<DbStats> {
    let data_dir = state.config.data_dir.clone();
    state
        .db
        .read(move |conn| {
            let reclaimable = pragma_i64(conn, "freelist_count")? * pragma_i64(conn, "page_size")?;
            let names: Vec<NameRow> = sql_query(
                "SELECT name FROM sqlite_master \
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
                   AND name <> '__diesel_schema_migrations'",
            )
            .load(conn)?;
            let mut tables = Vec::with_capacity(names.len());
            for NameRow { name } in names {
                // Table names come from sqlite_master, never from a request.
                let row: CountRow =
                    sql_query(format!("SELECT COUNT(*) AS n FROM \"{name}\"")).get_result(conn)?;
                tables.push(TableRows { name, rows: row.n });
            }
            tables.sort_by(|a, b| b.rows.cmp(&a.rows).then_with(|| a.name.cmp(&b.name)));
            Ok(DbStats {
                file_bytes: file_bytes(&data_dir),
                reclaimable_bytes: reclaimable.max(0) as u64,
                tables,
            })
        })
        .await
}

/// Free bytes on the filesystem holding `dir`, where the platform can say.
fn free_bytes(dir: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `path` is a valid NUL-terminated string and `st` a valid
        // out-pointer for the duration of the call.
        if unsafe { libc::statvfs(path.as_ptr(), &mut st) } != 0 {
            return None;
        }
        Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CompactReport {
    pub before_bytes: u64,
    pub after_bytes: u64,
}

/// Why a compaction was refused before it started.
#[derive(Debug, thiserror::Error)]
pub enum CompactError {
    #[error(
        "Not enough free disk space: compacting needs about {needed} bytes free, {free} available."
    )]
    NotEnoughSpace { needed: u64, free: u64 },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// `VACUUM` the database (rewrites the file without its free pages), fold
/// the write-ahead log back in, and refresh planner statistics. Holds the
/// shared connection for the whole run: every other database user waits.
/// `VACUUM` writes a full copy first, so it is refused when the disk has
/// less free space than the database's size.
pub async fn compact(state: &AppState) -> Result<CompactReport, CompactError> {
    let data_dir = state.config.data_dir.clone();
    let before = file_bytes(&data_dir);
    if let Some(free) = free_bytes(&data_dir)
        && free < before
    {
        return Err(CompactError::NotEnoughSpace {
            needed: before,
            free,
        });
    }
    state
        .db
        .with_conn(|conn| {
            sql_query("VACUUM").execute(conn)?;
            sql_query("PRAGMA wal_checkpoint(TRUNCATE)")
                .execute(conn)
                .ok();
            sql_query("PRAGMA optimize").execute(conn).ok();
            Ok(())
        })
        .await?;
    let after = file_bytes(&data_dir);
    tracing::info!(
        before_bytes = before,
        after_bytes = after,
        "Database compacted"
    );
    Ok(CompactReport {
        before_bytes: before,
        after_bytes: after,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CheckReport {
    pub ok: bool,
    /// SQLite's findings when `ok` is false (capped at 100 by SQLite).
    pub problems: Vec<String>,
}

/// `PRAGMA integrity_check` on a separate read connection.
pub async fn check(state: &AppState) -> anyhow::Result<CheckReport> {
    state
        .db
        .read(|conn| {
            #[derive(QueryableByName)]
            struct Line {
                #[diesel(sql_type = Text)]
                integrity_check: String,
            }
            let lines: Vec<Line> = sql_query("PRAGMA integrity_check").load(conn)?;
            let problems: Vec<String> = lines
                .into_iter()
                .map(|l| l.integrity_check)
                .filter(|l| l != "ok")
                .collect();
            Ok(CheckReport {
                ok: problems.is_empty(),
                problems,
            })
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{NewFolder, NewSession};

    fn exec(conn: &mut SqliteConnection, sql: &str) {
        sql_query(sql).execute(conn).unwrap();
    }

    fn rows_of(report: &CleanupReport, key: &str) -> i64 {
        report.items.iter().find(|i| i.key == key).unwrap().rows
    }

    async fn count(db: &Db, table: &'static str) -> i64 {
        db.with_conn(move |conn| {
            let row: CountRow =
                sql_query(format!("SELECT COUNT(*) AS n FROM {table}")).get_result(conn)?;
            Ok(row.n)
        })
        .await
        .unwrap()
    }

    /// Seeds one live and one orphaned row per rule, then checks the preview
    /// counts the orphans, deletes nothing, and the run removes exactly them.
    #[tokio::test]
    async fn cleanup_removes_orphans_and_expired_rows_and_nothing_else() {
        let tmp = tempfile::tempdir().unwrap();
        let state = crate::auth::middleware::tests::test_state(tmp.path());
        let now = chrono::Utc::now();
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
        let old = (now - chrono::Duration::hours(3)).to_rfc3339();
        for (id, is_temp, created) in [
            ("live", false, now.to_rfc3339()),
            ("temp-old", true, old.clone()),
            ("temp-new", true, now.to_rfc3339()),
        ] {
            state
                .db
                .create_session(NewSession {
                    id: id.into(),
                    name: id.into(),
                    folder_id: "f".into(),
                    is_temp,
                    created_at: created.clone(),
                    last_activity: created,
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        let secs = now.timestamp();
        let past = crate::db::crud::oauth_ts(now - chrono::Duration::hours(1));
        let future = crate::db::crud::oauth_ts(now + chrono::Duration::hours(1));
        state
            .db
            .with_conn(move |conn| {
                // Orphans can only be written with enforcement off: that is
                // how they got into old databases too.
                exec(conn, "PRAGMA foreign_keys=OFF");
                exec(
                    conn,
                    "INSERT INTO users (id, username, password_hash, role, created_at, updated_at) \
                     VALUES ('u', 'u', 'x', 'admin', 'now', 'now')",
                );
                for sid in ["live", "gone"] {
                    exec(
                        conn,
                        &format!(
                            "INSERT INTO events (id, session_id, seq, ts, kind, data) \
                             VALUES ('e-{sid}', '{sid}', 1, 1, 'user', '{{}}')"
                        ),
                    );
                    exec(
                        conn,
                        &format!(
                            "INSERT INTO queued_messages (session_id, text, queued_at) \
                             VALUES ('{sid}', 'hi', 'now')"
                        ),
                    );
                    exec(
                        conn,
                        &format!(
                            "INSERT INTO todos (session_id, position, content, status, updated_at) \
                             VALUES ('{sid}', 0, 'x', 'pending', 'now')"
                        ),
                    );
                    exec(
                        conn,
                        &format!(
                            "INSERT INTO plugin_session_meta (session_id, plugin_id, data, updated_at) \
                             VALUES ('{sid}', 'p', '{{}}', 'now')"
                        ),
                    );
                    exec(
                        conn,
                        &format!(
                            "INSERT INTO plans (id, session_id, title, markdown, status, version, created_at, updated_at) \
                             VALUES ('plan-{sid}', '{sid}', 't', 'm', 'draft', 1, 'now', 'now')"
                        ),
                    );
                    exec(
                        conn,
                        &format!(
                            "INSERT INTO voice_topics (id, session_id, kind, title, status, is_current, created_at, updated_at) \
                             VALUES ('vt-{sid}', '{sid}', 'conversation', 't', 'closed', 0, 'now', 'now')"
                        ),
                    );
                    exec(
                        conn,
                        &format!(
                            "INSERT INTO user_tabs (user_id, item_type, item_id, last_active) \
                             VALUES ('u', 'session', '{sid}', 'now')"
                        ),
                    );
                }
                // A report tab points at a file and must survive.
                exec(
                    conn,
                    "INSERT INTO user_tabs (user_id, item_type, item_id, last_active) \
                     VALUES ('u', 'report', 'some/report.md', 'now')",
                );
                for (id, expires) in [("a-live", secs + 600), ("a-dead", secs - 600)] {
                    exec(
                        conn,
                        &format!(
                            "INSERT INTO auth_sessions (id, user_id, token_hash, created_at, expires_at) \
                             VALUES ('{id}', 'u', 'h-{id}', 1, {expires})"
                        ),
                    );
                    exec(
                        conn,
                        &format!(
                            "INSERT INTO mfa_challenges (id, user_id, token_hash, created_at, expires_at, failures) \
                             VALUES ('{id}', 'u', 'h-{id}', 1, {expires}, 0)"
                        ),
                    );
                }
                for (hash, expires) in [("t-live", &future), ("t-dead", &past)] {
                    exec(
                        conn,
                        &format!(
                            "INSERT INTO oauth_access_tokens (token_hash, oauth_client_id, expires_at, created_at) \
                             VALUES ('{hash}', 'c', '{expires}', 'now')"
                        ),
                    );
                }
                exec(conn, "PRAGMA foreign_keys=ON");
                Ok(())
            })
            .await
            .unwrap();

        let preview = preview_cleanup(&state).await.unwrap();
        for key in [
            "events",
            "queued_messages",
            "todos",
            "plugin_session_meta",
            "plans",
            "voice_topics",
            "user_tabs",
            "auth_sessions",
            "mfa_challenges",
            "oauth_access_tokens",
            TEMP_SESSIONS_KEY,
        ] {
            assert_eq!(rows_of(&preview, key), 1, "preview of {key}");
        }
        assert_eq!(rows_of(&preview, "mfa_pending"), 0);
        assert_eq!(preview.total, 11);
        assert_eq!(
            count(&state.db, "events").await,
            2,
            "a preview deletes nothing"
        );

        let done = run_cleanup(&state).await.unwrap();
        assert_eq!(done, preview, "the run removes what the preview listed");
        for table in [
            "events",
            "queued_messages",
            "todos",
            "plugin_session_meta",
            "plans",
            "voice_topics",
            "auth_sessions",
            "mfa_challenges",
            "oauth_access_tokens",
        ] {
            assert_eq!(
                count(&state.db, table).await,
                1,
                "live row of {table} stays"
            );
        }
        // The live session's tab and the report tab stay.
        assert_eq!(count(&state.db, "user_tabs").await, 2);
        assert!(state.db.get_session("temp-old").await.unwrap().is_none());
        assert!(
            state.db.get_session("temp-new").await.unwrap().is_some(),
            "a temp session inside the grace period stays"
        );
        assert_eq!(preview_cleanup(&state).await.unwrap().total, 0);
    }

    /// A plan whose session is gone but whose card still exists is reachable
    /// from the card and must stay.
    #[tokio::test]
    async fn a_plan_with_a_live_card_survives_its_session() {
        let tmp = tempfile::tempdir().unwrap();
        let state = crate::auth::middleware::tests::test_state(tmp.path());
        state
            .db
            .with_conn(|conn| {
                exec(conn, "PRAGMA foreign_keys=OFF");
                exec(
                    conn,
                    "INSERT INTO cards (id, project_id, title, created_at, updated_at) \
                     VALUES ('c', 'p', 't', 'now', 'now')",
                );
                exec(
                    conn,
                    "INSERT INTO plans (id, session_id, card_id, title, markdown, status, version, created_at, updated_at) \
                     VALUES ('kept', 'gone', 'c', 't', 'm', 'draft', 1, 'now', 'now'), \
                            ('dropped', 'gone', 'gone-card', 't', 'm', 'draft', 1, 'now', 'now')",
                );
                exec(conn, "PRAGMA foreign_keys=ON");
                Ok(())
            })
            .await
            .unwrap();
        let done = run_cleanup(&state).await.unwrap();
        assert_eq!(rows_of(&done, "plans"), 1);
        assert_eq!(count(&state.db, "plans").await, 1);
    }

    #[tokio::test]
    async fn stats_check_and_compact_work_on_a_file_database() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(tmp.path()).unwrap();
        let remote_access = crate::service::remote_access::RemoteAccess::new(
            db.clone(),
            vec![0u8; 32],
            tmp.path(),
            std::sync::Arc::new(crate::service::remote_access::relay::RelayBackend),
        );
        let state = crate::auth::middleware::tests::test_state_with(tmp.path(), db, remote_access);
        // Grow the file, then free the pages, so there is something to give back.
        state
            .db
            .with_conn(|conn| {
                exec(
                    conn,
                    "INSERT INTO announcements (id, kind, title, message, created_at) \
                     WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2000) \
                     SELECT 'a' || i, 'k', 't', hex(randomblob(500)), 'now' FROM n",
                );
                Ok(())
            })
            .await
            .unwrap();
        let full = stats(&state).await.unwrap();
        assert_eq!(full.tables[0].name, "announcements");
        assert_eq!(full.tables[0].rows, 2000);
        state
            .db
            .with_conn(|conn| {
                exec(conn, "DELETE FROM announcements");
                Ok(())
            })
            .await
            .unwrap();
        assert!(stats(&state).await.unwrap().reclaimable_bytes > 500_000);

        let report = check(&state).await.unwrap();
        assert!(report.ok, "{:?}", report.problems);

        let compacted = compact(&state).await.unwrap();
        assert!(compacted.after_bytes < compacted.before_bytes);
        assert_eq!(stats(&state).await.unwrap().reclaimable_bytes, 0);
    }

    /// Each new index is the one SQLite picks for the lookup it was added
    /// for. Foreign-key lookups are `column = ?` probes, so the plain
    /// equality query stands in for them.
    #[tokio::test]
    async fn the_planner_uses_the_lookup_indexes() {
        #[derive(QueryableByName)]
        struct Plan {
            #[diesel(sql_type = Text)]
            detail: String,
        }
        let db = Db::in_memory().unwrap();
        let cases: &[(&str, &str)] = &[
            (
                "idx_usage_events_event",
                "SELECT 1 FROM usage_events WHERE event_id = 'x'",
            ),
            (
                "idx_usage_events_ts",
                "SELECT SUM(total_tokens) FROM usage_events WHERE ts >= 1 AND ts < 2",
            ),
            (
                "idx_cards_project",
                "SELECT * FROM cards WHERE project_id = 'x' ORDER BY priority, created_at",
            ),
            (
                "idx_cards_worker_session",
                "SELECT 1 FROM cards WHERE worker_session_id = 'x'",
            ),
            (
                "idx_cards_last_worker_session",
                "SELECT 1 FROM cards WHERE last_worker_session_id = 'x'",
            ),
            (
                "idx_sessions_card",
                "SELECT * FROM sessions WHERE is_worker = 1 AND card_id = 'x' \
                 ORDER BY last_activity DESC",
            ),
            (
                "idx_sessions_project",
                "SELECT * FROM sessions WHERE project_id = 'x'",
            ),
            (
                "idx_chat_focus_queue_source",
                "SELECT 1 FROM chat_focus_queue WHERE source_session_id = 'x'",
            ),
            (
                "idx_voice_relay_queue_source",
                "SELECT 1 FROM voice_relay_queue WHERE source_session_id = 'x'",
            ),
            (
                "idx_session_view_nodes_session",
                "SELECT 1 FROM session_view_nodes WHERE session_id = 'x'",
            ),
            (
                "idx_dashboard_widgets_session",
                "SELECT 1 FROM dashboard_widgets WHERE session_id = 'x'",
            ),
            (
                "idx_session_skills_session",
                "SELECT 1 FROM session_skills WHERE session_id = 'x'",
            ),
            (
                "idx_events_todo",
                "SELECT session_id, MAX(seq) FROM events WHERE kind = 'todo' GROUP BY session_id",
            ),
            (
                "idx_events_agent_usage",
                "SELECT e.id FROM events e WHERE e.kind = 'agent-usage' \
                 AND NOT EXISTS (SELECT 1 FROM usage_events u WHERE u.event_id = e.id)",
            ),
        ];
        assert_eq!(
            cases.len(),
            crate::db::repair::LOOKUP_INDEXES.len(),
            "one planner case per lookup index"
        );
        let plans = db
            .with_conn(move |conn| {
                let mut out = Vec::new();
                for (index, sql) in cases {
                    let rows: Vec<Plan> =
                        sql_query(format!("EXPLAIN QUERY PLAN {sql}")).load(conn)?;
                    let plan = rows
                        .into_iter()
                        .map(|r| r.detail)
                        .collect::<Vec<_>>()
                        .join(" | ");
                    out.push((*index, plan));
                }
                // The duplicate of the UNIQUE (session_id, seq) index is gone,
                // and the unique index still serves the chat page query.
                let dup: CountRow = sql_query(
                    "SELECT COUNT(*) AS n FROM sqlite_master \
                     WHERE type = 'index' AND name = 'idx_events_session'",
                )
                .get_result(conn)?;
                assert_eq!(dup.n, 0);
                let rows: Vec<Plan> = sql_query(
                    "EXPLAIN QUERY PLAN SELECT * FROM events \
                     WHERE session_id = 'x' AND seq < 5 ORDER BY seq DESC LIMIT 10",
                )
                .load(conn)?;
                assert!(
                    rows.iter()
                        .any(|r| r.detail.contains("sqlite_autoindex_events")),
                    "events page query lost its index"
                );
                Ok(out)
            })
            .await
            .unwrap();
        for (index, plan) in plans {
            assert!(plan.contains(index), "{index} not used: {plan}");
        }
    }
}
