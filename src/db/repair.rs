//! Schema-drift repair that runs after diesel migrations.
//!
//! Why this exists: SQLite doesn't support `ALTER TABLE … ADD COLUMN
//! IF NOT EXISTS`, and a botched migration can leave older data dirs
//! missing columns the code now requires. Re-running the original
//! migration fails on healthy dirs (column exists) and only works on
//! broken ones. So instead we check the live schema and patch what's
//! missing, idempotently.
//!
//! Every patch here MUST be safe to run on a fresh, fully-migrated DB —
//! i.e. detect-then-skip rather than detect-then-fail. New entries
//! should be tied to the bug that motivated them in a comment so we
//! can prune them once enough time has passed.

use diesel::prelude::*;
use diesel::sql_query;
use diesel::sqlite::SqliteConnection;

/// Heal any known schema drift. Idempotent. Called at startup right
/// after `run_pending_migrations`.
pub fn ensure_schema(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    ensure_projects_worker_communication_columns(conn)?;
    ensure_queued_messages_model_columns(conn)?;
    ensure_card_dependencies_table(conn)?;
    ensure_todos_table(conn)?;
    ensure_cards_completed_at_column(conn)?;
    ensure_sessions_expert_columns(conn)?;
    ensure_repeating_tasks_schema(conn)?;
    Ok(())
}

/// Heal DBs that predate `1780985065_expert_sessions`. That migration is
/// a series of bare `ALTER TABLE … ADD COLUMN` statements (SQLite has no
/// IF NOT EXISTS for ADD COLUMN), so on a DB that already has the columns
/// it would fail — this detect-then-skip path is the only safe way to add
/// them to an older data dir. All columns are additive with DEFAULTs or
/// NULL-able, so existing rows need no backfill.
fn ensure_sessions_expert_columns(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let rows: Vec<PragmaColumn> = sql_query("PRAGMA table_info(sessions)").load(conn)?;
    let existing: Vec<String> = rows.into_iter().map(|r| r.name).collect();
    if existing.is_empty() {
        // Table itself missing — migrations haven't run. Don't ALTER;
        // let the caller surface the schema-missing error.
        return Ok(());
    }
    // (column name, full type+default clause) for each additive column.
    let columns = [
        ("is_expert", "BOOLEAN NOT NULL DEFAULT 0"),
        ("expert_kind", "TEXT"),
        ("knowledge_summary", "TEXT"),
        ("knowledge_area", "TEXT"),
        ("scope_path", "TEXT"),
        ("is_permanent", "BOOLEAN NOT NULL DEFAULT 0"),
    ];
    for (name, clause) in columns {
        if !existing.iter().any(|c| c == name) {
            tracing::info!("Repairing schema: adding sessions.{name}");
            sql_query(format!("ALTER TABLE sessions ADD COLUMN {name} {clause}")).execute(conn)?;
        }
    }
    Ok(())
}

/// Heal DBs that predate `1781025117_repeating_tasks`. The migration
/// creates the table with `IF NOT EXISTS` (safe to re-run) and adds a
/// non-idempotent `ALTER TABLE sessions ADD COLUMN repeating_task_id`,
/// which we detect-and-add here for any DB that ran the table-creation
/// half but not the column-add half.
///
/// Bails out cleanly if the prerequisite tables (`folders`, `sessions`)
/// don't exist yet. Real DBs always have them, but test fixtures that
/// build a minimal schema can hit this path before any migrations have
/// created sessions/folders, and a hard failure here would mask the
/// fixture's intent.
fn ensure_repeating_tasks_schema(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let folders_cols: Vec<PragmaColumn> = sql_query("PRAGMA table_info(folders)").load(conn)?;
    let sessions_cols: Vec<PragmaColumn> = sql_query("PRAGMA table_info(sessions)").load(conn)?;
    if folders_cols.is_empty() || sessions_cols.is_empty() {
        return Ok(());
    }

    sql_query(
        "CREATE TABLE IF NOT EXISTS repeating_tasks (
            id              TEXT    PRIMARY KEY NOT NULL,
            name            TEXT    NOT NULL,
            description     TEXT    NOT NULL DEFAULT '',
            folder_id       TEXT    NOT NULL REFERENCES folders(id),
            prompt          TEXT    NOT NULL,
            schedule_kind   TEXT    NOT NULL,
            schedule_value  TEXT    NOT NULL,
            model           TEXT,
            effort          TEXT,
            enabled         BOOLEAN NOT NULL DEFAULT 1,
            next_run_at     TEXT,
            last_run_at     TEXT,
            created_at      TEXT    NOT NULL,
            updated_at      TEXT    NOT NULL
        )",
    )
    .execute(conn)?;
    sql_query(
        "CREATE INDEX IF NOT EXISTS idx_repeating_tasks_folder ON repeating_tasks (folder_id)",
    )
    .execute(conn)?;
    sql_query(
        "CREATE INDEX IF NOT EXISTS idx_repeating_tasks_next_run \
         ON repeating_tasks (next_run_at) WHERE enabled = 1",
    )
    .execute(conn)?;

    // ALTER TABLE sessions ADD COLUMN -- the only non-idempotent part of
    // the migration. Skip if the column is already present.
    let existing: Vec<String> = sessions_cols.into_iter().map(|r| r.name).collect();
    if !existing.iter().any(|c| c == "repeating_task_id") {
        tracing::info!("Repairing schema: adding sessions.repeating_task_id");
        sql_query(
            "ALTER TABLE sessions ADD COLUMN repeating_task_id TEXT REFERENCES repeating_tasks(id)",
        )
        .execute(conn)?;
    }
    sql_query(
        "CREATE INDEX IF NOT EXISTS idx_sessions_repeating_task ON sessions (repeating_task_id)",
    )
    .execute(conn)?;
    Ok(())
}

/// Heal DBs that predate `1780966657_cards_completed_at` AND backfill
/// `completed_at` for cards already in `done`. The migration is a bare
/// `ALTER TABLE … ADD COLUMN` (SQLite has no IF NOT EXISTS for that),
/// so this is the only safe path on a DB that already has the column.
///
/// Backfill uses `updated_at` as the best available proxy for "when did
/// this card finish" on legacy rows — the DB doesn't preserve transition
/// timestamps. Re-running is safe: we only touch rows where
/// `completed_at IS NULL AND step = 'done'`, so post-migration writes
/// (which carry an accurate timestamp) are never clobbered.
fn ensure_cards_completed_at_column(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let rows: Vec<PragmaColumn> = sql_query("PRAGMA table_info(cards)").load(conn)?;
    let existing: Vec<String> = rows.into_iter().map(|r| r.name).collect();
    if existing.is_empty() {
        return Ok(());
    }
    let needs_add = !existing.iter().any(|c| c == "completed_at");
    if needs_add {
        tracing::info!("Repairing schema: adding cards.completed_at");
        sql_query("ALTER TABLE cards ADD COLUMN completed_at TEXT").execute(conn)?;
    }
    // Backfill once: if we just added the column there are no
    // post-migration writes to protect; on healthy DBs the column is
    // already accurate so we don't touch existing rows.
    if needs_add {
        sql_query(
            "UPDATE cards
             SET completed_at = updated_at
             WHERE completed_at IS NULL AND step = 'done'",
        )
        .execute(conn)?;
    }
    Ok(())
}

/// Heal DBs that predate (or somehow skipped) the
/// `1780883838_card_dependencies` migration. `CREATE TABLE IF NOT
/// EXISTS` is inherently idempotent, so this is safe on a fully-migrated
/// DB and only does work on one that lacks the table.
fn ensure_card_dependencies_table(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    sql_query(
        "CREATE TABLE IF NOT EXISTS card_dependencies (
            card_id             TEXT NOT NULL REFERENCES cards(id) ON DELETE CASCADE,
            depends_on_card_id  TEXT NOT NULL REFERENCES cards(id) ON DELETE CASCADE,
            created_at          TEXT NOT NULL,
            PRIMARY KEY (card_id, depends_on_card_id)
        )",
    )
    .execute(conn)?;
    Ok(())
}

/// Heal DBs that predate the `1780900501_todos` migration AND backfill
/// the new `todos` table from each session's most recent `todo` event,
/// so an older DB doesn't lose its current snapshot when the read path
/// switches over from `latest_event_of_kind`.
///
/// Idempotent in both directions:
///   * `CREATE TABLE IF NOT EXISTS` is a no-op on healthy DBs.
///   * Backfill replaces each session's rows with whatever the latest
///     `todo` event says — so re-running just re-asserts the same state.
///     Sessions that received fresh writes after startup will already
///     hold the post-startup snapshot; those won't have a stale
///     pre-startup `todo` event later than the live writes, so re-runs
///     don't clobber newer data.
fn ensure_todos_table(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    sql_query(
        "CREATE TABLE IF NOT EXISTS todos (
            session_id   TEXT    NOT NULL,
            position     INTEGER NOT NULL,
            content      TEXT    NOT NULL,
            status       TEXT    NOT NULL,
            active_form  TEXT,
            updated_at   TEXT    NOT NULL,
            PRIMARY KEY (session_id, position)
        )",
    )
    .execute(conn)?;
    sql_query("CREATE INDEX IF NOT EXISTS idx_todos_session ON todos (session_id)")
        .execute(conn)?;
    backfill_todos_from_events(conn)?;
    Ok(())
}

#[derive(QueryableByName, Debug)]
struct SessionTodoEvent {
    #[diesel(sql_type = diesel::sql_types::Text)]
    session_id: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    data: String,
}

/// Backfill: for every session whose latest `todo` event isn't already
/// reflected in `todos`, replace that session's rows. We skip sessions
/// that already have rows whose `updated_at` is newer than the event's
/// timestamp — those got a fresh write at runtime and we mustn't
/// clobber them on a later restart.
fn backfill_todos_from_events(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    // Skip if the `events` table doesn't exist yet (e.g. test fixtures
    // that build a minimal schema). Real DBs always have it; this is a
    // belt-and-braces guard for the repair-tests-only case.
    let has_events: Vec<PragmaColumn> = sql_query("PRAGMA table_info(events)").load(conn)?;
    if has_events.is_empty() {
        return Ok(());
    }

    // Latest `todo` event per session.
    let rows: Vec<SessionTodoEvent> = sql_query(
        "SELECT e.session_id AS session_id, e.data AS data
         FROM events e
         JOIN (
             SELECT session_id, MAX(seq) AS max_seq
             FROM events
             WHERE kind = 'todo'
             GROUP BY session_id
         ) latest
           ON latest.session_id = e.session_id
          AND latest.max_seq = e.seq
         WHERE e.kind = 'todo'",
    )
    .load(conn)?;

    if rows.is_empty() {
        return Ok(());
    }

    let now = chrono::Utc::now().to_rfc3339();
    for row in rows {
        // Skip if this session already has todos rows — runtime writes win.
        let existing: i64 = sql_query("SELECT COUNT(*) AS n FROM todos WHERE session_id = ?1")
            .bind::<diesel::sql_types::Text, _>(&row.session_id)
            .get_result::<CountRow>(conn)
            .map(|r| r.n)
            .unwrap_or(0);
        if existing > 0 {
            continue;
        }

        let Ok(data) = serde_json::from_str::<serde_json::Value>(&row.data) else {
            continue;
        };
        let Some(arr) = data.get("todos").and_then(|v| v.as_array()) else {
            continue;
        };

        tracing::info!(
            session_id = %row.session_id,
            count = arr.len(),
            "Backfilling todos table from latest event"
        );

        for (position, item) in arr.iter().enumerate() {
            let content = item
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let status = item
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("pending")
                .to_string();
            let active_form = item
                .get("activeForm")
                .and_then(|v| v.as_str())
                .map(str::to_string);

            sql_query(
                "INSERT OR REPLACE INTO todos
                   (session_id, position, content, status, active_form, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .bind::<diesel::sql_types::Text, _>(&row.session_id)
            .bind::<diesel::sql_types::Integer, _>(position as i32)
            .bind::<diesel::sql_types::Text, _>(&content)
            .bind::<diesel::sql_types::Text, _>(&status)
            .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(&active_form)
            .bind::<diesel::sql_types::Text, _>(&now)
            .execute(conn)?;
        }
    }

    Ok(())
}

#[derive(QueryableByName, Debug)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
}

/// Original bug: `00000000000002_user_tabs` (since renamed to
/// `00000000000003_`) collided with the upstream
/// `00000000000002_worker_communication`. Diesel records migrations by
/// numeric version; with the collision it marked version `2` applied
/// after running ONE of the two SQL files. DBs created in that window
/// are missing the two columns the worker_communication migration was
/// supposed to add.
fn ensure_projects_worker_communication_columns(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let existing = project_columns(conn)?;
    if !existing.iter().any(|c| c == "auto_notify_changes") {
        tracing::info!("Repairing schema: adding projects.auto_notify_changes");
        sql_query("ALTER TABLE projects ADD COLUMN auto_notify_changes BOOLEAN NOT NULL DEFAULT 1")
            .execute(conn)?;
    }
    if !existing.iter().any(|c| c == "worker_communication") {
        tracing::info!("Repairing schema: adding projects.worker_communication");
        sql_query(
            "ALTER TABLE projects ADD COLUMN worker_communication BOOLEAN NOT NULL DEFAULT 1",
        )
        .execute(conn)?;
    }
    Ok(())
}

#[derive(QueryableByName, Debug)]
struct PragmaColumn {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
}

fn project_columns(conn: &mut SqliteConnection) -> anyhow::Result<Vec<String>> {
    let rows: Vec<PragmaColumn> = sql_query("PRAGMA table_info(projects)").load(conn)?;
    Ok(rows.into_iter().map(|r| r.name).collect())
}

/// Backfill for the `model` / `effort` columns added to `queued_messages`
/// in migration `1780879129_queued_message_model`. Migration is additive
/// (NULL-able columns), but ALTER ADD COLUMN is not idempotent in SQLite,
/// so DBs that somehow skipped the migration get healed here.
fn ensure_queued_messages_model_columns(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let rows: Vec<PragmaColumn> = sql_query("PRAGMA table_info(queued_messages)").load(conn)?;
    let existing: Vec<String> = rows.into_iter().map(|r| r.name).collect();
    if existing.is_empty() {
        // Table itself missing — migrations haven't run. Don't try to
        // ALTER; let the caller surface the schema-missing error.
        return Ok(());
    }
    if !existing.iter().any(|c| c == "model") {
        tracing::info!("Repairing schema: adding queued_messages.model");
        sql_query("ALTER TABLE queued_messages ADD COLUMN model TEXT").execute(conn)?;
    }
    if !existing.iter().any(|c| c == "effort") {
        tracing::info!("Repairing schema: adding queued_messages.effort");
        sql_query("ALTER TABLE queued_messages ADD COLUMN effort TEXT").execute(conn)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use diesel::Connection;

    /// Simulate a DB created during the collision window: same tables
    /// as today but without the two project columns. ensure_schema
    /// must add them.
    #[test]
    fn ensure_schema_adds_missing_project_columns() {
        let mut conn = SqliteConnection::establish(":memory:").unwrap();

        sql_query("CREATE TABLE projects (id TEXT PRIMARY KEY NOT NULL, name TEXT NOT NULL)")
            .execute(&mut conn)
            .unwrap();
        let before = project_columns(&mut conn).unwrap();
        assert!(!before.iter().any(|c| c == "auto_notify_changes"));

        ensure_schema(&mut conn).unwrap();

        let after = project_columns(&mut conn).unwrap();
        assert!(
            after.iter().any(|c| c == "auto_notify_changes"),
            "got columns {:?}",
            after,
        );
        assert!(after.iter().any(|c| c == "worker_communication"));
    }

    /// Pre-existing DB has no `todos` table and a session whose latest
    /// `todo` event holds the live snapshot. ensure_schema must create
    /// the table AND backfill rows from that event.
    #[test]
    fn ensure_schema_backfills_todos_from_latest_event() {
        let mut conn = SqliteConnection::establish(":memory:").unwrap();

        // Other ensure_schema steps prod `projects` and `queued_messages`;
        // stub the bare minimum so we can isolate the todos check.
        sql_query(
            "CREATE TABLE projects (
                id TEXT PRIMARY KEY NOT NULL,
                auto_notify_changes BOOLEAN NOT NULL DEFAULT 1,
                worker_communication BOOLEAN NOT NULL DEFAULT 1
            )",
        )
        .execute(&mut conn)
        .unwrap();
        sql_query(
            "CREATE TABLE queued_messages (
                session_id TEXT PRIMARY KEY NOT NULL,
                text TEXT NOT NULL,
                queued_at TEXT NOT NULL,
                model TEXT,
                effort TEXT
            )",
        )
        .execute(&mut conn)
        .unwrap();

        // Minimal `events` shape — enough for the backfill query.
        sql_query(
            "CREATE TABLE events (
                id TEXT PRIMARY KEY NOT NULL,
                session_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                ts BIGINT NOT NULL,
                kind TEXT NOT NULL,
                data TEXT NOT NULL
            )",
        )
        .execute(&mut conn)
        .unwrap();

        // Two `todo` events for one session — backfill must pick the
        // latest by seq, not seq=1's stale snapshot.
        sql_query(
            "INSERT INTO events (id, session_id, seq, ts, kind, data) VALUES
             ('e1', 's1', 1, 100, 'todo',
                '{\"todos\":[{\"content\":\"stale\",\"status\":\"pending\"}]}'),
             ('e2', 's1', 2, 200, 'todo',
                '{\"todos\":[{\"content\":\"latest a\",\"status\":\"in_progress\",\"activeForm\":\"Doing a\"},{\"content\":\"latest b\",\"status\":\"done\"}]}')",
        )
        .execute(&mut conn)
        .unwrap();

        ensure_schema(&mut conn).unwrap();

        #[derive(QueryableByName, Debug)]
        struct R {
            #[diesel(sql_type = diesel::sql_types::Text)]
            content: String,
            #[diesel(sql_type = diesel::sql_types::Text)]
            status: String,
            #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
            active_form: Option<String>,
        }

        let rows: Vec<R> = sql_query(
            "SELECT content, status, active_form
             FROM todos WHERE session_id='s1' ORDER BY position",
        )
        .load(&mut conn)
        .unwrap();

        assert_eq!(rows.len(), 2, "stale event must not be picked");
        assert_eq!(rows[0].content, "latest a");
        assert_eq!(rows[0].status, "in_progress");
        assert_eq!(rows[0].active_form.as_deref(), Some("Doing a"));
        assert_eq!(rows[1].content, "latest b");
        assert_eq!(rows[1].status, "done");

        // Second call must be a no-op — existing rows win, no clobber.
        ensure_schema(&mut conn).unwrap();
        let rows2: Vec<R> = sql_query(
            "SELECT content, status, active_form
             FROM todos WHERE session_id='s1' ORDER BY position",
        )
        .load(&mut conn)
        .unwrap();
        assert_eq!(rows2.len(), 2);
    }

    /// Running on a healthy schema must be a no-op (no double-add).
    #[test]
    fn ensure_schema_is_idempotent() {
        let mut conn = SqliteConnection::establish(":memory:").unwrap();
        sql_query(
            "CREATE TABLE projects (
                id TEXT PRIMARY KEY NOT NULL,
                auto_notify_changes BOOLEAN NOT NULL DEFAULT 1,
                worker_communication BOOLEAN NOT NULL DEFAULT 1
            )",
        )
        .execute(&mut conn)
        .unwrap();

        ensure_schema(&mut conn).unwrap();
        ensure_schema(&mut conn).unwrap(); // second call must not error
    }

    /// A DB that predates `1780985065_expert_sessions` has a `sessions`
    /// table without the expert columns. ensure_schema must add every
    /// column AND preserve existing rows (no data loss). Idempotent on a
    /// second run.
    #[test]
    fn ensure_schema_adds_missing_session_expert_columns() {
        let mut conn = SqliteConnection::establish(":memory:").unwrap();

        // Other ensure_schema steps prod these tables; stub the minimum.
        sql_query(
            "CREATE TABLE projects (
                id TEXT PRIMARY KEY NOT NULL,
                auto_notify_changes BOOLEAN NOT NULL DEFAULT 1,
                worker_communication BOOLEAN NOT NULL DEFAULT 1
            )",
        )
        .execute(&mut conn)
        .unwrap();

        // Pre-migration sessions shape with a single existing row.
        sql_query(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY NOT NULL,
                name TEXT NOT NULL,
                is_worker BOOLEAN NOT NULL DEFAULT 0
            )",
        )
        .execute(&mut conn)
        .unwrap();
        sql_query("INSERT INTO sessions (id, name, is_worker) VALUES ('s1', 'Chat', 0)")
            .execute(&mut conn)
            .unwrap();

        ensure_schema(&mut conn).unwrap();

        let cols: Vec<String> = {
            let rows: Vec<PragmaColumn> = sql_query("PRAGMA table_info(sessions)")
                .load(&mut conn)
                .unwrap();
            rows.into_iter().map(|r| r.name).collect()
        };
        for expected in [
            "is_expert",
            "expert_kind",
            "knowledge_summary",
            "knowledge_area",
            "scope_path",
            "is_permanent",
        ] {
            assert!(
                cols.iter().any(|c| c == expected),
                "missing {expected}; got {cols:?}",
            );
        }

        // Existing row survived and defaults applied.
        #[derive(QueryableByName)]
        struct Row {
            #[diesel(sql_type = diesel::sql_types::Text)]
            name: String,
            #[diesel(sql_type = diesel::sql_types::Bool)]
            is_expert: bool,
            #[diesel(sql_type = diesel::sql_types::Bool)]
            is_permanent: bool,
        }
        let rows: Vec<Row> =
            sql_query("SELECT name, is_expert, is_permanent FROM sessions WHERE id = 's1'")
                .load(&mut conn)
                .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "Chat");
        assert!(!rows[0].is_expert);
        assert!(!rows[0].is_permanent);

        // Second run must be a no-op (no double-add error).
        ensure_schema(&mut conn).unwrap();
    }
}
