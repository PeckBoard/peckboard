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
    Ok(())
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
}
