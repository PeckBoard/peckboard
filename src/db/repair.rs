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
    ensure_queued_messages_fifo_shape(conn)?;
    ensure_card_dependencies_table(conn)?;
    ensure_todos_table(conn)?;
    ensure_cards_completed_at_column(conn)?;
    ensure_sessions_expert_columns(conn)?;
    ensure_repeating_tasks_schema(conn)?;
    ensure_sessions_pagination_indexes(conn)?;
    ensure_projects_workflow_column(conn)?;
    ensure_custom_workflows_tables(conn)?;
    ensure_cards_workflow_column(conn)?;
    ensure_projects_pause_reason_column(conn)?;
    ensure_project_workflow_instructions_table(conn)?;
    ensure_plugin_settings_table(conn)?;
    ensure_plugin_approvals_table(conn)?;
    ensure_plugin_repositories_table(conn)?;
    ensure_pm_decisions_table(conn)?;
    ensure_usage_events_table(conn)?;
    ensure_usage_events_account_id_column(conn)?;
    ensure_claude_accounts_table(conn)?;
    ensure_claude_accounts_token_columns(conn)?;
    ensure_grok_accounts_table(conn)?;
    ensure_kimi_accounts_table(conn)?;
    ensure_user_tabs_check_constraint(conn)?;
    ensure_plugin_data_tables(conn)?;
    ensure_sessions_system_prompt_column(conn)?;
    ensure_sessions_handover_columns(conn)?;
    ensure_sessions_worker_step_column(conn)?;
    ensure_sessions_user_id_column(conn)?;
    ensure_sessions_context_reset_column(conn)?;
    ensure_sessions_parent_link_columns(conn)?;
    ensure_model_autoswitch_columns(conn)?;
    ensure_system_prompt_name_columns(conn)?;
    ensure_system_prompts_table(conn)?;
    ensure_plans_tables(conn)?;
    ensure_projects_worktree_isolation_column(conn)?;
    ensure_cards_worktree_unmerged_columns(conn)?;
    ensure_projects_budget_columns(conn)?;
    ensure_sessions_pending_plan_review_column(conn)?;
    ensure_doc_review_tables(conn)?;
    ensure_sessions_pending_doc_review_column(conn)?;
    ensure_sessions_is_temp_column(conn)?;
    ensure_repeating_tasks_timezone_column(conn)?;
    ensure_repeating_task_runs_table(conn)?;
    ensure_env_vars_table(conn)?;
    ensure_agent_vars_table(conn)?;
    backfill_session_owners(conn)?;
    Ok(())
}

/// Heal DBs that predate `1782694873_claude_accounts`. The migration adds
/// a non-idempotent `ALTER TABLE usage_events ADD COLUMN account_id`,
/// detected-and-added here for any DB that somehow missed it. Plain
/// nullable column (no FK), matching the migration — the delete path nulls
/// orphaned rows itself.
fn ensure_usage_events_account_id_column(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let rows: Vec<PragmaColumn> = sql_query("PRAGMA table_info(usage_events)").load(conn)?;
    let existing: Vec<String> = rows.into_iter().map(|r| r.name).collect();
    if existing.is_empty() {
        return Ok(());
    }
    if !existing.iter().any(|c| c == "account_id") {
        tracing::info!("Repairing schema: adding usage_events.account_id");
        sql_query("ALTER TABLE usage_events ADD COLUMN account_id TEXT").execute(conn)?;
        sql_query(
            "CREATE INDEX IF NOT EXISTS idx_usage_events_account \
             ON usage_events (account_id, ts)",
        )
        .execute(conn)?;
    }
    Ok(())
}

/// Heal DBs that predate `1782694873_claude_accounts`. `CREATE TABLE IF
/// NOT EXISTS` is idempotent so this is safe on a fully-migrated DB and
/// only does work on one that lacks the table. DDL mirrors the migration.
fn ensure_claude_accounts_table(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    log_if_healing_table(conn, "claude_accounts")?;
    sql_query(
        "CREATE TABLE IF NOT EXISTS claude_accounts (
            id                  TEXT    PRIMARY KEY NOT NULL,
            name                TEXT    NOT NULL,
            kind                TEXT    NOT NULL,
            credential          TEXT    NOT NULL,
            config_dir          TEXT,
            budget_window_hours INTEGER,
            budget_limit_usd    REAL,
            budget_limit_tokens INTEGER,
            warn_threshold      REAL    NOT NULL DEFAULT 0.75,
            critical_threshold  REAL    NOT NULL DEFAULT 0.90,
            created_at          BIGINT  NOT NULL,
            updated_at          BIGINT  NOT NULL,
            refresh_token       TEXT,
            token_expires_at    BIGINT
        )",
    )
    .execute(conn)?;
    Ok(())
}

/// Heal DBs that predate `1783600000_claude_account_token_refresh`. The
/// migration's `ALTER TABLE … ADD COLUMN` is non-idempotent, so the two
/// columns are detected-and-added here, mirroring the migration.
fn ensure_claude_accounts_token_columns(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let rows: Vec<PragmaColumn> = sql_query("PRAGMA table_info(claude_accounts)").load(conn)?;
    let existing: Vec<String> = rows.into_iter().map(|r| r.name).collect();
    if existing.is_empty() {
        return Ok(());
    }
    for (col, ddl) in [
        (
            "refresh_token",
            "ALTER TABLE claude_accounts ADD COLUMN refresh_token TEXT",
        ),
        (
            "token_expires_at",
            "ALTER TABLE claude_accounts ADD COLUMN token_expires_at BIGINT",
        ),
    ] {
        if !existing.iter().any(|c| c == col) {
            tracing::info!("Repairing schema: adding claude_accounts.{col}");
            sql_query(ddl).execute(conn)?;
        }
    }
    Ok(())
}

/// Heal DBs that predate `1782713952_grok_accounts`. `CREATE TABLE IF NOT
/// EXISTS` is idempotent so this is safe on a fully-migrated DB and only does
/// work on one that lacks the table. DDL mirrors the migration.
fn ensure_grok_accounts_table(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    log_if_healing_table(conn, "grok_accounts")?;
    sql_query(
        "CREATE TABLE IF NOT EXISTS grok_accounts (
            id                  TEXT    PRIMARY KEY NOT NULL,
            name                TEXT    NOT NULL,
            kind                TEXT    NOT NULL,
            credential          TEXT    NOT NULL,
            config_dir          TEXT,
            budget_window_hours INTEGER,
            budget_limit_usd    REAL,
            budget_limit_tokens INTEGER,
            warn_threshold      REAL    NOT NULL DEFAULT 0.75,
            critical_threshold  REAL    NOT NULL DEFAULT 0.90,
            created_at          BIGINT  NOT NULL,
            updated_at          BIGINT  NOT NULL
        )",
    )
    .execute(conn)?;
    Ok(())
}

/// Heal DBs that predate `1784300000_kimi_accounts`. `CREATE TABLE IF NOT
/// EXISTS` is idempotent so this is safe on a fully-migrated DB and only does
/// work on one that lacks the table. DDL mirrors the migration.
fn ensure_kimi_accounts_table(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    log_if_healing_table(conn, "kimi_accounts")?;
    sql_query(
        "CREATE TABLE IF NOT EXISTS kimi_accounts (
            id                  TEXT    PRIMARY KEY NOT NULL,
            name                TEXT    NOT NULL,
            kind                TEXT    NOT NULL,
            credential          TEXT    NOT NULL,
            config_dir          TEXT,
            budget_window_hours INTEGER,
            budget_limit_usd    REAL,
            budget_limit_tokens INTEGER,
            warn_threshold      REAL    NOT NULL DEFAULT 0.75,
            critical_threshold  REAL    NOT NULL DEFAULT 0.90,
            created_at          BIGINT  NOT NULL,
            updated_at          BIGINT  NOT NULL
        )",
    )
    .execute(conn)?;
    Ok(())
}

/// Heal DBs that predate `1781682475_plugin_data_and_session_meta`. Both are
/// `CREATE TABLE/INDEX IF NOT EXISTS`, idempotent on a fully-migrated DB; DDL
/// mirrors the migration. Backs the generic plugin document store +
/// per-session plugin metadata used by `src/plugin/host.rs`.
fn ensure_plugin_data_tables(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    log_if_healing_table(conn, "plugin_data")?;
    sql_query(
        "CREATE TABLE IF NOT EXISTS plugin_data (
            plugin_id   TEXT NOT NULL,
            collection  TEXT NOT NULL,
            key         TEXT NOT NULL,
            data        TEXT NOT NULL,
            created_at  TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at  TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (plugin_id, collection, key)
        )",
    )
    .execute(conn)?;
    sql_query(
        "CREATE TABLE IF NOT EXISTS plugin_session_meta (
            session_id  TEXT NOT NULL,
            plugin_id   TEXT NOT NULL,
            data        TEXT NOT NULL,
            updated_at  TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (session_id, plugin_id)
        )",
    )
    .execute(conn)?;
    sql_query(
        "CREATE INDEX IF NOT EXISTS idx_plugin_session_meta_plugin \
         ON plugin_session_meta (plugin_id)",
    )
    .execute(conn)?;
    Ok(())
}

/// Heal DBs that predate `1781120408_usage_events`. `CREATE TABLE IF NOT
/// EXISTS` + `CREATE INDEX IF NOT EXISTS` no-op on a healthy DB; the
/// `WHERE NOT EXISTS`-guarded backfill rebuilds any usage row whose live
/// mirror-write was lost. DDL + backfill mirror the migration. Runs every
/// startup, so the guard keeps the warm path cheap. (mirrors the
/// cards_completed_at structure-then-backfill heal pattern.)
fn ensure_usage_events_table(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    // `usage_events` FKs onto `sessions` and `events` and backfills from
    // `events`, so it's only meaningful once the base schema exists. Skip
    // entirely on the minimal partial schemas the repair tests build (and
    // any conceivable pre-migration-1 DB). Real DBs always have both.
    let has_sessions: Vec<PragmaColumn> = sql_query("PRAGMA table_info(sessions)").load(conn)?;
    let has_events: Vec<PragmaColumn> = sql_query("PRAGMA table_info(events)").load(conn)?;
    if has_sessions.is_empty() || has_events.is_empty() {
        return Ok(());
    }
    log_if_healing_table(conn, "usage_events")?;
    sql_query(
        "CREATE TABLE IF NOT EXISTS usage_events (
            id                    TEXT    PRIMARY KEY NOT NULL,
            session_id            TEXT    NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            event_id              TEXT    REFERENCES events(id) ON DELETE SET NULL,
            turn_seq              INTEGER,
            ts                    INTEGER NOT NULL,
            input_tokens          INTEGER NOT NULL DEFAULT 0,
            output_tokens         INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
            cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
            total_tokens          INTEGER NOT NULL DEFAULT 0,
            context_tokens        INTEGER NOT NULL DEFAULT 0,
            model                 TEXT
        )",
    )
    .execute(conn)?;
    sql_query(
        "CREATE INDEX IF NOT EXISTS idx_usage_events_session \
         ON usage_events (session_id, ts)",
    )
    .execute(conn)?;
    sql_query(
        "INSERT INTO usage_events (
            id, session_id, event_id, turn_seq, ts,
            input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
            total_tokens, context_tokens, model
        )
        SELECT
            lower(hex(randomblob(16))),
            e.session_id,
            e.id,
            ROW_NUMBER() OVER (PARTITION BY e.session_id ORDER BY e.ts, e.seq),
            e.ts,
            COALESCE(json_extract(e.data, '$.inputTokens'), 0),
            COALESCE(json_extract(e.data, '$.outputTokens'), 0),
            COALESCE(json_extract(e.data, '$.cacheReadTokens'), 0),
            COALESCE(json_extract(e.data, '$.cacheCreationTokens'), 0),
            COALESCE(json_extract(e.data, '$.totalTokens'), 0),
            COALESCE(json_extract(e.data, '$.contextTokens'), 0),
            json_extract(e.data, '$.model')
        FROM events e
        WHERE e.kind = 'agent-usage'
          AND NOT EXISTS (SELECT 1 FROM usage_events u WHERE u.event_id = e.id)",
    )
    .execute(conn)?;
    Ok(())
}

/// Heal DBs that predate `1781074848_pm_decisions`. `CREATE TABLE IF
/// NOT EXISTS` is idempotent so this is safe on a fully-migrated DB and
/// only does work on one that lacks the table. DDL mirrors the
/// migration byte-for-byte.
fn ensure_pm_decisions_table(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    log_if_healing_table(conn, "pm_decisions")?;
    sql_query(
        "CREATE TABLE IF NOT EXISTS pm_decisions (
            id                   TEXT PRIMARY KEY NOT NULL,
            project_id           TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            question             TEXT NOT NULL,
            answer               TEXT,
            status               TEXT NOT NULL DEFAULT 'pending',
            asked_by_session_id  TEXT,
            superseded_by        TEXT REFERENCES pm_decisions(id),
            created_at           TEXT NOT NULL,
            answered_at          TEXT
        )",
    )
    .execute(conn)?;
    sql_query(
        "CREATE INDEX IF NOT EXISTS idx_pm_decisions_project_status \
         ON pm_decisions (project_id, status)",
    )
    .execute(conn)?;
    Ok(())
}

/// Heal DBs that predate `1781075129_plugin_settings`. Idempotent —
/// `CREATE TABLE IF NOT EXISTS` no-ops on a healthy DB.
fn ensure_plugin_settings_table(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    log_if_healing_table(conn, "plugin_settings")?;
    sql_query(
        "CREATE TABLE IF NOT EXISTS plugin_settings (
            plugin_id   TEXT NOT NULL,
            key         TEXT NOT NULL,
            value       TEXT NOT NULL,
            updated_at  TEXT NOT NULL,
            PRIMARY KEY (plugin_id, key)
        )",
    )
    .execute(conn)?;
    sql_query(
        "CREATE INDEX IF NOT EXISTS idx_plugin_settings_plugin \
         ON plugin_settings (plugin_id)",
    )
    .execute(conn)?;
    Ok(())
}

/// Heal DBs that predate `1781586748_plugin_approvals`. `CREATE TABLE IF
/// NOT EXISTS` no-ops on a healthy DB.
fn ensure_plugin_approvals_table(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    log_if_healing_table(conn, "plugin_approvals")?;
    sql_query(
        "CREATE TABLE IF NOT EXISTS plugin_approvals (
            plugin_id   TEXT NOT NULL PRIMARY KEY,
            hooks       TEXT NOT NULL,
            status      TEXT NOT NULL,
            decided_at  TEXT NOT NULL
        )",
    )
    .execute(conn)?;
    Ok(())
}

/// Heal DBs that predate `1781592551_plugin_repositories`. Creates the
/// table only — the default-repo seed lives in the migration so it runs
/// exactly once (a removed default must stay removed); re-seeding here
/// every startup would resurrect it.
fn ensure_plugin_repositories_table(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    log_if_healing_table(conn, "plugin_repositories")?;
    sql_query(
        "CREATE TABLE IF NOT EXISTS plugin_repositories (
            url        TEXT NOT NULL PRIMARY KEY,
            label      TEXT NOT NULL,
            added_at   TEXT NOT NULL
        )",
    )
    .execute(conn)?;
    Ok(())
}

/// Heal DBs that predate `1781062932_project_workflow_instructions`.
/// `CREATE TABLE IF NOT EXISTS` is idempotent so this is safe on a
/// fully-migrated DB and only does work on one that lacks the table.
fn ensure_project_workflow_instructions_table(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    log_if_healing_table(conn, "project_workflow_instructions")?;
    sql_query(
        "CREATE TABLE IF NOT EXISTS project_workflow_instructions (
            project_id   TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            workflow_id  TEXT NOT NULL,
            step         TEXT NOT NULL,
            instructions TEXT NOT NULL,
            created_at   TEXT NOT NULL,
            updated_at   TEXT NOT NULL,
            PRIMARY KEY (project_id, workflow_id, step)
        )",
    )
    .execute(conn)?;
    sql_query(
        "CREATE INDEX IF NOT EXISTS idx_pwi_project \
         ON project_workflow_instructions (project_id)",
    )
    .execute(conn)?;
    Ok(())
}
/// Heal DBs that predate `1786200000_custom_workflows`.
fn ensure_custom_workflows_tables(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    log_if_healing_table(conn, "custom_workflows")?;
    sql_query(
        "CREATE TABLE IF NOT EXISTS custom_workflows (
            id           TEXT PRIMARY KEY,
            name         TEXT NOT NULL,
            description  TEXT NOT NULL,
            priority     INTEGER NOT NULL,
            created_at   TEXT NOT NULL,
            updated_at   TEXT NOT NULL
        )",
    )
    .execute(conn)?;
    sql_query(
        "CREATE TABLE IF NOT EXISTS custom_workflow_steps (
            workflow_id  TEXT NOT NULL REFERENCES custom_workflows(id) ON DELETE CASCADE,
            position     INTEGER NOT NULL,
            step         TEXT NOT NULL,
            instructions TEXT NOT NULL,
            PRIMARY KEY (workflow_id, position)
        )",
    )
    .execute(conn)?;
    sql_query(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_custom_workflow_steps_step \
         ON custom_workflow_steps (workflow_id, step)",
    )
    .execute(conn)?;
    Ok(())
}

/// Heal DBs that predate `1781058245_projects_pause_reason`. The migration
/// is a bare `ALTER TABLE … ADD COLUMN` (SQLite has no IF NOT EXISTS for
/// that), so this detect-then-skip path is the only safe way to add the
/// column to an older data dir. NULL-able with no backfill.
fn ensure_projects_pause_reason_column(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let existing = project_columns(conn)?;
    if existing.is_empty() {
        return Ok(());
    }
    if !existing.iter().any(|c| c == "pause_reason") {
        tracing::info!("Repairing schema: adding projects.pause_reason");
        sql_query("ALTER TABLE projects ADD COLUMN pause_reason TEXT").execute(conn)?;
    }
    Ok(())
}

/// Composite indexes for keyset-paginated session lists. Mirrors
/// `migrations/1781033682_session_pagination_indexes` so a DB that
/// somehow skipped that migration still gets the planner support the
/// route assumes. `CREATE INDEX IF NOT EXISTS` is idempotent, so this
/// is safe to call on a healthy schema.
fn ensure_sessions_pagination_indexes(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let cols: Vec<PragmaColumn> = sql_query("PRAGMA table_info(sessions)").load(conn)?;
    if cols.is_empty() {
        return Ok(());
    }
    // Both indexes reference `last_activity` and (one of them) `folder_id`.
    // The repair-tests stub a minimal sessions table without those columns,
    // and a real DB that pre-dates `00000000000001_initial` should never
    // get here — but bail out cleanly rather than fail with a confusing
    // "no such column" error if either prerequisite is missing.
    let names: Vec<String> = cols.into_iter().map(|c| c.name).collect();
    let has = |n: &str| names.iter().any(|c| c == n);
    if !has("last_activity") || !has("id") {
        return Ok(());
    }
    sql_query(
        "CREATE INDEX IF NOT EXISTS idx_sessions_last_activity ON sessions (last_activity, id)",
    )
    .execute(conn)?;
    if has("folder_id") {
        sql_query(
            "CREATE INDEX IF NOT EXISTS idx_sessions_folder_last_activity \
             ON sessions (folder_id, last_activity, id)",
        )
        .execute(conn)?;
    }
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

/// Heal DBs that predate the `session_system_prompt` migration, whose sole
/// statement is a non-idempotent `ALTER TABLE sessions ADD COLUMN
/// system_prompt`. Detect-then-add so a DB that already has the column (or a
/// fresh DB where the migration ran) is left untouched. Additive + nullable,
/// so no backfill is needed.
fn ensure_sessions_system_prompt_column(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let rows: Vec<PragmaColumn> = sql_query("PRAGMA table_info(sessions)").load(conn)?;
    let existing: Vec<String> = rows.into_iter().map(|r| r.name).collect();
    if existing.is_empty() {
        // Table missing — migrations haven't run; let the caller surface it.
        return Ok(());
    }
    if !existing.iter().any(|c| c == "system_prompt") {
        tracing::info!("Repairing schema: adding sessions.system_prompt");
        sql_query("ALTER TABLE sessions ADD COLUMN system_prompt TEXT").execute(conn)?;
    }
    Ok(())
}

/// Heal DBs that predate the `session_handover` migration (and the later
/// `session_handover_run_id` one), whose statements are non-idempotent
/// `ALTER TABLE sessions ADD COLUMN`s. Each is detected-and-added
/// independently so a DB that ran one but not the others still converges.
/// All are additive + nullable, so no backfill.
fn ensure_sessions_handover_columns(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let rows: Vec<PragmaColumn> = sql_query("PRAGMA table_info(sessions)").load(conn)?;
    let existing: Vec<String> = rows.into_iter().map(|r| r.name).collect();
    if existing.is_empty() {
        return Ok(());
    }
    if !existing.iter().any(|c| c == "handover_to_model") {
        tracing::info!("Repairing schema: adding sessions.handover_to_model");
        sql_query("ALTER TABLE sessions ADD COLUMN handover_to_model TEXT").execute(conn)?;
    }
    if !existing.iter().any(|c| c == "pending_handover_doc") {
        tracing::info!("Repairing schema: adding sessions.pending_handover_doc");
        sql_query("ALTER TABLE sessions ADD COLUMN pending_handover_doc TEXT").execute(conn)?;
    }
    if !existing.iter().any(|c| c == "handover_run_id") {
        tracing::info!("Repairing schema: adding sessions.handover_run_id");
        sql_query("ALTER TABLE sessions ADD COLUMN handover_run_id BIGINT").execute(conn)?;
    }
    Ok(())
}
/// Heal DBs that predate the `session_worker_step` migration, whose single
/// statement is a non-idempotent `ALTER TABLE sessions ADD COLUMN`.
/// Additive + nullable, so no backfill.
fn ensure_sessions_worker_step_column(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let rows: Vec<PragmaColumn> = sql_query("PRAGMA table_info(sessions)").load(conn)?;
    let existing: Vec<String> = rows.into_iter().map(|r| r.name).collect();
    if existing.is_empty() {
        return Ok(());
    }
    if !existing.iter().any(|c| c == "worker_step") {
        tracing::info!("Repairing schema: adding sessions.worker_step");
        sql_query("ALTER TABLE sessions ADD COLUMN worker_step TEXT").execute(conn)?;
    }
    Ok(())
}

/// Heal DBs that predate `1783300000_session_user_id`. Adds the nullable,
/// FK-less `sessions.user_id` owner column (mirrors the migration) for any DB
/// that missed it. Backfill is handled separately by `backfill_session_owners`
/// so it also runs on already-migrated DBs.
fn ensure_sessions_user_id_column(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let rows: Vec<PragmaColumn> = sql_query("PRAGMA table_info(sessions)").load(conn)?;
    let existing: Vec<String> = rows.into_iter().map(|r| r.name).collect();
    if existing.is_empty() {
        return Ok(());
    }
    if !existing.iter().any(|c| c == "user_id") {
        tracing::info!("Repairing schema: adding sessions.user_id");
        sql_query("ALTER TABLE sessions ADD COLUMN user_id TEXT").execute(conn)?;
    }
    Ok(())
}

/// Heal DBs that predate `1783500000_session_context_reset`, whose single
/// statement is a non-idempotent `ALTER TABLE sessions ADD COLUMN`.
/// Additive + nullable, so no backfill.
fn ensure_sessions_context_reset_column(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let rows: Vec<PragmaColumn> = sql_query("PRAGMA table_info(sessions)").load(conn)?;
    let existing: Vec<String> = rows.into_iter().map(|r| r.name).collect();
    if existing.is_empty() {
        return Ok(());
    }
    if !existing.iter().any(|c| c == "context_reset_ts") {
        tracing::info!("Repairing schema: adding sessions.context_reset_ts");
        sql_query("ALTER TABLE sessions ADD COLUMN context_reset_ts BIGINT").execute(conn)?;
    }
    Ok(())
}

/// Heal DBs that predate `1784200000_session_parent_link`, whose two
/// `ALTER TABLE sessions ADD COLUMN` statements are non-idempotent. Both
/// columns are additive + nullable TEXT (NULL `parent_session_id` = not a
/// subagent; NULL `subagent_completed_at` = still running), so no backfill.
/// The partial index mirrors the migration.
fn ensure_sessions_parent_link_columns(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let rows: Vec<PragmaColumn> = sql_query("PRAGMA table_info(sessions)").load(conn)?;
    let existing: Vec<String> = rows.into_iter().map(|r| r.name).collect();
    if existing.is_empty() {
        return Ok(());
    }
    for col in ["parent_session_id", "subagent_completed_at"] {
        if !existing.iter().any(|c| c == col) {
            tracing::info!("Repairing schema: adding sessions.{col}");
            sql_query(format!("ALTER TABLE sessions ADD COLUMN {col} TEXT")).execute(conn)?;
        }
    }
    sql_query(
        "CREATE INDEX IF NOT EXISTS idx_sessions_parent_session_id \
         ON sessions(parent_session_id) WHERE parent_session_id IS NOT NULL",
    )
    .execute(conn)?;
    Ok(())
}

/// Heal DBs that predate `1783700000_model_autoswitch`. The migration's two
/// `ALTER TABLE … ADD COLUMN model_autoswitch` statements are non-idempotent,
/// so each is detected-and-added here. Nullable BOOLEAN (NULL = inherit the
/// per-session default), mirroring the migration.
fn ensure_model_autoswitch_columns(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    for table in ["sessions", "cards"] {
        let rows: Vec<PragmaColumn> =
            sql_query(format!("PRAGMA table_info({table})")).load(conn)?;
        let existing: Vec<String> = rows.into_iter().map(|r| r.name).collect();
        if existing.is_empty() {
            continue;
        }
        if !existing.iter().any(|c| c == "model_autoswitch") {
            tracing::info!("Repairing schema: adding {table}.model_autoswitch");
            sql_query(format!(
                "ALTER TABLE {table} ADD COLUMN model_autoswitch BOOLEAN"
            ))
            .execute(conn)?;
        }
    }
    Ok(())
}

/// Heal DBs that predate `1783800000_system_prompt_name`. The migration's two
/// `ALTER TABLE … ADD COLUMN system_prompt_name` statements are non-idempotent,
/// so each is detected-and-added here. Nullable TEXT (NULL = no named library
/// prompt selected), mirroring the migration.
fn ensure_system_prompt_name_columns(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    for table in ["sessions", "cards"] {
        let rows: Vec<PragmaColumn> =
            sql_query(format!("PRAGMA table_info({table})")).load(conn)?;
        let existing: Vec<String> = rows.into_iter().map(|r| r.name).collect();
        if existing.is_empty() {
            continue;
        }
        if !existing.iter().any(|c| c == "system_prompt_name") {
            tracing::info!("Repairing schema: adding {table}.system_prompt_name");
            sql_query(format!(
                "ALTER TABLE {table} ADD COLUMN system_prompt_name TEXT"
            ))
            .execute(conn)?;
        }
    }
    Ok(())
}
/// Heal DBs that predate `1786100000_cards_worktree_unmerged`. Both
/// `ALTER TABLE cards ADD COLUMN` statements are non-idempotent, so each is
/// detected-and-added here. Nullable TEXT (NULL reason = no unmerged
/// worktree pending), mirroring the migration.
fn ensure_cards_worktree_unmerged_columns(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let rows: Vec<PragmaColumn> = sql_query("PRAGMA table_info(cards)").load(conn)?;
    let existing: Vec<String> = rows.into_iter().map(|r| r.name).collect();
    if existing.is_empty() {
        return Ok(());
    }
    for col in ["worktree_unmerged_reason", "worktree_unmerged_detail"] {
        if !existing.iter().any(|c| c == col) {
            tracing::info!("Repairing schema: adding cards.{col}");
            sql_query(format!("ALTER TABLE cards ADD COLUMN {col} TEXT")).execute(conn)?;
        }
    }
    Ok(())
}
/// Heal DBs that predate `1783700001_system_prompts`. `CREATE TABLE IF NOT
/// EXISTS` is idempotent so this is safe on a fully-migrated DB and only does
/// work on one that lacks the table. DDL mirrors the migration.
fn ensure_system_prompts_table(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    log_if_healing_table(conn, "system_prompts")?;
    sql_query(
        "CREATE TABLE IF NOT EXISTS system_prompts (
            id          TEXT PRIMARY KEY NOT NULL,
            name        TEXT NOT NULL UNIQUE,
            body        TEXT NOT NULL,
            source_url  TEXT,
            created_at  TEXT NOT NULL,
            updated_at  TEXT NOT NULL
        )",
    )
    .execute(conn)?;
    Ok(())
}

/// Heal DBs that predate `1783900000_plans`. Idempotent CREATE TABLE IF NOT
/// EXISTS + indexes for the plan store; DDL mirrors the migration.
fn ensure_plans_tables(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    log_if_healing_table(conn, "plans")?;
    sql_query(
        "CREATE TABLE IF NOT EXISTS plans (
            id          TEXT PRIMARY KEY NOT NULL,
            session_id  TEXT NOT NULL,
            card_id     TEXT,
            project_id  TEXT,
            title       TEXT NOT NULL,
            markdown    TEXT NOT NULL,
            status      TEXT NOT NULL DEFAULT 'proposed',
            version     INTEGER NOT NULL DEFAULT 1,
            created_at  TEXT NOT NULL,
            updated_at  TEXT NOT NULL
        )",
    )
    .execute(conn)?;
    sql_query("CREATE INDEX IF NOT EXISTS idx_plans_session ON plans (session_id)")
        .execute(conn)?;
    sql_query("CREATE INDEX IF NOT EXISTS idx_plans_card ON plans (card_id)").execute(conn)?;
    Ok(())
}

/// Heal DBs that predate `1783900000_plans`: the one-shot review-injection
/// flag on sessions. NOT NULL DEFAULT 0 mirrors the migration.
fn ensure_sessions_pending_plan_review_column(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let rows: Vec<PragmaColumn> = sql_query("PRAGMA table_info(sessions)").load(conn)?;
    let existing: Vec<String> = rows.into_iter().map(|r| r.name).collect();
    if existing.is_empty() {
        return Ok(());
    }
    if !existing.iter().any(|c| c == "pending_plan_review") {
        tracing::info!("Repairing schema: adding sessions.pending_plan_review");
        sql_query("ALTER TABLE sessions ADD COLUMN pending_plan_review BOOLEAN NOT NULL DEFAULT 0")
            .execute(conn)?;
    }
    Ok(())
}

/// Heal DBs that predate `1785100000_doc_reviews`. Mirrors the migration's
/// tables verbatim, including the FKs the cascade paths rely on.
fn ensure_doc_review_tables(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    log_if_healing_table(conn, "doc_reviews")?;
    sql_query(
        "CREATE TABLE IF NOT EXISTS doc_reviews (
            id              TEXT    PRIMARY KEY NOT NULL,
            title           TEXT    NOT NULL,
            source_kind     TEXT    NOT NULL CHECK (source_kind IN ('file', 'report', 'plan')),
            source_ref      TEXT    NOT NULL,
            folder_id       TEXT    REFERENCES folders(id),
            project_id      TEXT,
            session_id      TEXT,
            status          TEXT    NOT NULL DEFAULT 'annotating',
            current_version INTEGER NOT NULL DEFAULT 1,
            created_at      TEXT    NOT NULL,
            updated_at      TEXT    NOT NULL
        )",
    )
    .execute(conn)?;
    sql_query("CREATE INDEX IF NOT EXISTS idx_doc_reviews_status ON doc_reviews (status)")
        .execute(conn)?;
    sql_query("CREATE INDEX IF NOT EXISTS idx_doc_reviews_session ON doc_reviews (session_id)")
        .execute(conn)?;
    sql_query("CREATE INDEX IF NOT EXISTS idx_doc_reviews_folder ON doc_reviews (folder_id)")
        .execute(conn)?;
    sql_query(
        "CREATE TABLE IF NOT EXISTS doc_review_versions (
            review_id  TEXT    NOT NULL REFERENCES doc_reviews(id) ON DELETE CASCADE,
            version    INTEGER NOT NULL,
            markdown   TEXT    NOT NULL,
            note       TEXT    NOT NULL,
            created_by TEXT    NOT NULL CHECK (created_by IN ('user', 'assistant')),
            created_at TEXT    NOT NULL,
            PRIMARY KEY (review_id, version)
        )",
    )
    .execute(conn)?;
    sql_query(
        "CREATE INDEX IF NOT EXISTS idx_doc_review_versions_review ON doc_review_versions (review_id)",
    )
    .execute(conn)?;
    sql_query(
        "CREATE TABLE IF NOT EXISTS doc_review_comments (
            id              TEXT    PRIMARY KEY NOT NULL,
            review_id       TEXT    NOT NULL REFERENCES doc_reviews(id) ON DELETE CASCADE,
            version         INTEGER NOT NULL,
            start_line      INTEGER NOT NULL,
            end_line        INTEGER NOT NULL,
            quote           TEXT,
            kind            TEXT    NOT NULL CHECK (kind IN ('comment', 'suggest', 'wrong', 'expand', 'shorten')),
            body            TEXT    NOT NULL,
            status          TEXT    NOT NULL DEFAULT 'pending',
            resolution_note TEXT,
            created_at      TEXT    NOT NULL
        )",
    )
    .execute(conn)?;
    sql_query(
        "CREATE INDEX IF NOT EXISTS idx_doc_review_comments_review ON doc_review_comments (review_id)",
    )
    .execute(conn)?;
    sql_query(
        "CREATE INDEX IF NOT EXISTS idx_doc_review_comments_status ON doc_review_comments (review_id, status)",
    )
    .execute(conn)?;
    // `1785200001_doc_review_pr_links` adds these two with bare, non-idempotent
    // ALTERs, and the CREATE TABLE above is a no-op on a DB that already has the
    // table — so they are detected-and-added separately.
    let comment_cols: Vec<String> = {
        let rows: Vec<PragmaColumn> =
            sql_query("PRAGMA table_info(doc_review_comments)").load(conn)?;
        rows.into_iter().map(|r| r.name).collect()
    };
    for col in ["external_kind", "external_id"] {
        if !comment_cols.iter().any(|c| c == col) {
            tracing::info!("Repairing schema: adding doc_review_comments.{col}");
            sql_query(format!(
                "ALTER TABLE doc_review_comments ADD COLUMN {col} TEXT"
            ))
            .execute(conn)?;
        }
    }
    // NULLs compare distinct in SQLite, so hand-written annotations still
    // insert freely; only imported ids are held unique per review.
    sql_query(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_doc_review_comments_external \
         ON doc_review_comments (review_id, external_id)",
    )
    .execute(conn)?;
    log_if_healing_table(conn, "doc_review_pr_links")?;
    sql_query(
        "CREATE TABLE IF NOT EXISTS doc_review_pr_links (
            review_id      TEXT    PRIMARY KEY NOT NULL REFERENCES doc_reviews(id) ON DELETE CASCADE,
            owner          TEXT    NOT NULL,
            repo           TEXT    NOT NULL,
            number         INTEGER NOT NULL,
            file_path      TEXT    NOT NULL,
            last_synced_at TEXT,
            created_at     TEXT    NOT NULL
        )",
    )
    .execute(conn)?;
    Ok(())
}

/// Heal DBs that predate `1785100000_doc_reviews`: the one-shot review
/// injection flag on sessions. Nullable TEXT (a review id) mirrors the
/// migration.
fn ensure_sessions_pending_doc_review_column(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let rows: Vec<PragmaColumn> = sql_query("PRAGMA table_info(sessions)").load(conn)?;
    let existing: Vec<String> = rows.into_iter().map(|r| r.name).collect();
    if existing.is_empty() {
        return Ok(());
    }
    if !existing.iter().any(|c| c == "pending_doc_review") {
        tracing::info!("Repairing schema: adding sessions.pending_doc_review");
        sql_query("ALTER TABLE sessions ADD COLUMN pending_doc_review TEXT").execute(conn)?;
    }
    Ok(())
}
/// Heal DBs that predate `1784100000_temp_sessions`: the auto-delete-on-
/// last-tab-close flag on sessions. NOT NULL DEFAULT 0 mirrors the migration.
fn ensure_sessions_is_temp_column(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let rows: Vec<PragmaColumn> = sql_query("PRAGMA table_info(sessions)").load(conn)?;
    let existing: Vec<String> = rows.into_iter().map(|r| r.name).collect();
    if existing.is_empty() {
        return Ok(());
    }
    if !existing.iter().any(|c| c == "is_temp") {
        tracing::info!("Repairing schema: adding sessions.is_temp");
        sql_query("ALTER TABLE sessions ADD COLUMN is_temp BOOLEAN NOT NULL DEFAULT 0")
            .execute(conn)?;
    }
    Ok(())
}
/// Backfill session ownership for single-operator installs: when the DB holds
/// exactly one user, every still-unowned session becomes theirs. Multi-user
/// installs are left untouched (ambiguous -- the operator resolves them). No-op
/// once every row is owned; idempotent, safe on every boot. Mirrors the
/// `1783300000_session_user_id` migration so it also heals rows that predate it.
fn backfill_session_owners(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    if !table_exists(conn, "users")? {
        return Ok(());
    }
    let cols: Vec<PragmaColumn> = sql_query("PRAGMA table_info(sessions)").load(conn)?;
    if !cols.iter().any(|c| c.name == "user_id") {
        return Ok(());
    }
    let user_count: i64 = sql_query("SELECT COUNT(*) AS n FROM users")
        .get_result::<CountRow>(conn)?
        .n;
    if user_count != 1 {
        return Ok(());
    }
    let updated =
        sql_query("UPDATE sessions SET user_id = (SELECT id FROM users) WHERE user_id IS NULL")
            .execute(conn)?;
    if updated > 0 {
        tracing::info!("Backfilled {updated} session owner(s) to the sole user");
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

    log_if_healing_table(conn, "repeating_tasks")?;
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
    log_if_healing_table(conn, "card_dependencies")?;
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
    log_if_healing_table(conn, "todos")?;
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
fn ensure_projects_worktree_isolation_column(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let existing = project_columns(conn)?;
    if !existing.iter().any(|c| c == "worktree_isolation") {
        tracing::info!("Repairing schema: adding projects.worktree_isolation");
        sql_query("ALTER TABLE projects ADD COLUMN worktree_isolation BOOLEAN NOT NULL DEFAULT 0")
            .execute(conn)?;
    }
    Ok(())
}

/// Heal DBs where `1783950000_project_budgets` was skipped or half-applied
/// (e.g. a duplicate-version collision): both columns are nullable, so
/// adding them here is always safe.
fn ensure_projects_budget_columns(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let existing = project_columns(conn)?;
    if !existing.iter().any(|c| c == "budget_usd_cents") {
        tracing::info!("Repairing schema: adding projects.budget_usd_cents");
        sql_query("ALTER TABLE projects ADD COLUMN budget_usd_cents INTEGER").execute(conn)?;
    }
    if !existing.iter().any(|c| c == "budget_period") {
        tracing::info!("Repairing schema: adding projects.budget_period");
        sql_query("ALTER TABLE projects ADD COLUMN budget_period TEXT").execute(conn)?;
    }
    Ok(())
}

/// Heal DBs that predate `1781054693_cards_workflow_required`. That
/// migration renames the legacy nullable `cards.workflow` column to
/// `workflow_legacy` and adds a NOT NULL `workflow` column with a
/// per-row backfill (card legacy value → owning project's workflow →
/// 'task'). Neither rename nor ADD COLUMN can be made idempotent in
/// SQLite, so the detect-then-skip path is the only safe way to apply
/// this to an older data dir.
fn ensure_cards_workflow_column(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let cols: Vec<(String, bool)> = sql_query(
        "SELECT name AS name, CAST(\"notnull\" AS INTEGER) AS not_null \
         FROM pragma_table_info('cards')",
    )
    .load::<NotNullColumn>(conn)?
    .into_iter()
    .map(|c| (c.name, c.not_null != 0))
    .collect();
    if cols.is_empty() {
        return Ok(());
    }
    let has = |n: &str| cols.iter().any(|(name, _)| name == n);
    let workflow_not_null = cols
        .iter()
        .find(|(name, _)| name == "workflow")
        .map(|(_, nn)| *nn)
        .unwrap_or(false);

    // Healthy: the migration has run, `workflow` exists with NOT NULL,
    // and the legacy column is preserved alongside.
    if has("workflow") && workflow_not_null && has("workflow_legacy") {
        return Ok(());
    }

    // Decide what to add and whether to backfill. The backfill only runs
    // when we touch the schema — on a healthy DB we exit above without
    // ever rewriting the live `workflow` values.
    let mut needs_backfill = false;
    if has("workflow_legacy") && !has("workflow") {
        // Resumed mid-state from a crash between RENAME and ADD COLUMN.
        sql_query("ALTER TABLE cards ADD COLUMN workflow TEXT NOT NULL DEFAULT 'task'")
            .execute(conn)?;
        needs_backfill = true;
    } else if !has("workflow_legacy") && has("workflow") && !workflow_not_null {
        // Legacy schema: nullable `workflow` only. Rename + add + backfill.
        tracing::info!("Repairing schema: renaming cards.workflow → workflow_legacy");
        sql_query("ALTER TABLE cards RENAME COLUMN workflow TO workflow_legacy").execute(conn)?;
        sql_query("ALTER TABLE cards ADD COLUMN workflow TEXT NOT NULL DEFAULT 'task'")
            .execute(conn)?;
        needs_backfill = true;
    } else if !has("workflow") {
        // Cards table predates having a workflow column at all (vanishingly
        // rare). Additive ADD is safe; no legacy values to backfill from.
        sql_query("ALTER TABLE cards ADD COLUMN workflow TEXT NOT NULL DEFAULT 'task'")
            .execute(conn)?;
        needs_backfill = true;
    }

    if needs_backfill {
        // Prefer the card's legacy value, fall back to the owning project's
        // workflow, then 'task'. Only rows that landed on the ADD COLUMN
        // default ('task') get re-evaluated; if a legitimate post-heal write
        // already set the value, we wouldn't be here (healthy-path early
        // exit catches that).
        sql_query(
            "UPDATE cards
                SET workflow = COALESCE(
                    NULLIF(workflow_legacy, ''),
                    (SELECT workflow FROM projects WHERE projects.id = cards.project_id),
                    'task'
                )",
        )
        .execute(conn)?;
    }

    Ok(())
}

#[derive(QueryableByName, Debug)]
struct NotNullColumn {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    not_null: i32,
}

/// Heal DBs that predate `1781053574_projects_workflow_required`. That
/// migration is a bare `ALTER TABLE … ADD COLUMN` plus a backfill UPDATE;
/// ADD COLUMN cannot be made idempotent in SQLite, so this detect-then-skip
/// path is the only safe way to add the column to an older data dir.
///
/// The new column is `NOT NULL DEFAULT 'task'`; the backfill copies the
/// project's previously-stored `default_workflow` whenever it was set.
fn ensure_projects_workflow_column(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let existing = project_columns(conn)?;
    if existing.is_empty() {
        return Ok(());
    }
    if !existing.iter().any(|c| c == "workflow") {
        tracing::info!("Repairing schema: adding projects.workflow");
        sql_query("ALTER TABLE projects ADD COLUMN workflow TEXT NOT NULL DEFAULT 'task'")
            .execute(conn)?;
        if existing.iter().any(|c| c == "default_workflow") {
            sql_query(
                "UPDATE projects \
                    SET workflow = default_workflow \
                  WHERE default_workflow IS NOT NULL \
                    AND default_workflow != ''",
            )
            .execute(conn)?;
        }
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

fn table_exists(conn: &mut SqliteConnection, table: &str) -> anyhow::Result<bool> {
    let rows: Vec<PragmaColumn> = sql_query(format!("PRAGMA table_info({table})")).load(conn)?;
    Ok(!rows.is_empty())
}

/// Log when a `CREATE TABLE IF NOT EXISTS` heal is about to do real work.
/// The ALTER-based heals already log per column; without this the
/// table-creation heals run silently, and a botched migration on a real
/// DB goes unnoticed until something else breaks.
fn log_if_healing_table(conn: &mut SqliteConnection, table: &str) -> anyhow::Result<()> {
    if !table_exists(conn, table)? {
        tracing::info!("Repairing schema: creating missing table {table}");
    }
    Ok(())
}

/// Heal DBs that predate `1781202566_user_tabs_more_kinds` or
/// `1785100001_user_tabs_doc_review`. Those migrations widen the
/// `user_tabs.item_type` CHECK constraint to allow `'report'`,
/// `'repeating_task'` and `'doc_review'` in addition to `'session'`
/// and `'project'`. CHECK constraints can only be changed by recreating
/// the table, and SQLite has no IF-CHECK-IS-RELAXED guard, so this
/// detect-then-recreate path heals data dirs that somehow skipped a
/// migration — or where the table-recreate half failed midway and left
/// an older CHECK in place.
fn ensure_user_tabs_check_constraint(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    #[derive(QueryableByName)]
    struct MasterRow {
        #[diesel(sql_type = diesel::sql_types::Text)]
        sql: String,
    }
    let row: Option<MasterRow> =
        sql_query("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'user_tabs'")
            .get_result(conn)
            .optional()?;
    let sql = match row {
        Some(r) => r.sql,
        // No user_tabs table at all — base migration hasn't run; let the
        // caller surface that rather than try to create one here.
        None => return Ok(()),
    };
    // Already at the widest CHECK (or never had one) — nothing to do.
    if sql.contains("'doc_review'") || !sql.contains("CHECK") {
        return Ok(());
    }
    // CHECK predates one of the widening migrations. Recreate the table
    // with the current CHECK, preserving every existing row.
    tracing::info!("Repairing schema: relaxing user_tabs.item_type CHECK constraint");
    sql_query(
        "CREATE TABLE IF NOT EXISTS user_tabs_new (
            user_id     TEXT    NOT NULL REFERENCES users(id),
            item_type   TEXT    NOT NULL CHECK (item_type IN ('session', 'project', 'report', 'repeating_task', 'doc_review')),
            item_id     TEXT    NOT NULL,
            last_active TEXT    NOT NULL,
            PRIMARY KEY (user_id, item_type, item_id)
        )",
    )
    .execute(conn)?;
    sql_query(
        "INSERT INTO user_tabs_new (user_id, item_type, item_id, last_active)
            SELECT user_id, item_type, item_id, last_active FROM user_tabs",
    )
    .execute(conn)?;
    sql_query("DROP TABLE user_tabs").execute(conn)?;
    sql_query("ALTER TABLE user_tabs_new RENAME TO user_tabs").execute(conn)?;
    sql_query(
        "CREATE INDEX IF NOT EXISTS idx_user_tabs_user_active ON user_tabs (user_id, last_active DESC)",
    )
    .execute(conn)?;
    Ok(())
}

/// Heal `queued_messages` to the FIFO shape from migration
/// `1785340000_queued_messages_fifo` (id PK + attachment_ids +
/// user_event_appended). Handles every older shape: the original
/// single-slot table (PK session_id), with or without the model/effort
/// columns from `1780879129_queued_message_model`. A rebuild (not ALTER)
/// because SQLite can't change a table's primary key in place.
fn ensure_queued_messages_fifo_shape(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let rows: Vec<PragmaColumn> = sql_query("PRAGMA table_info(queued_messages)").load(conn)?;
    let existing: Vec<String> = rows.into_iter().map(|r| r.name).collect();
    if existing.is_empty() {
        // Table itself missing — migrations haven't run. Don't try to
        // ALTER; let the caller surface the schema-missing error.
        return Ok(());
    }
    let has = |c: &str| existing.iter().any(|e| e == c);
    if !has("id") {
        tracing::info!("Repairing schema: rebuilding queued_messages as FIFO");
        let model_sel = if has("model") { "model" } else { "NULL" };
        let effort_sel = if has("effort") { "effort" } else { "NULL" };
        sql_query(
            "CREATE TABLE queued_messages_fifo (
                id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id          TEXT    NOT NULL,
                text                TEXT    NOT NULL,
                queued_at           TEXT    NOT NULL,
                model               TEXT,
                effort              TEXT,
                attachment_ids      TEXT,
                user_event_appended INTEGER NOT NULL DEFAULT 0
            )",
        )
        .execute(conn)?;
        sql_query(format!(
            "INSERT INTO queued_messages_fifo
                (session_id, text, queued_at, model, effort, user_event_appended)
             SELECT session_id, text, queued_at, {model_sel}, {effort_sel}, 1
             FROM queued_messages"
        ))
        .execute(conn)?;
        sql_query("DROP TABLE queued_messages").execute(conn)?;
        sql_query("ALTER TABLE queued_messages_fifo RENAME TO queued_messages").execute(conn)?;
    } else {
        if !has("attachment_ids") {
            tracing::info!("Repairing schema: adding queued_messages.attachment_ids");
            sql_query("ALTER TABLE queued_messages ADD COLUMN attachment_ids TEXT")
                .execute(conn)?;
        }
        if !has("user_event_appended") {
            tracing::info!("Repairing schema: adding queued_messages.user_event_appended");
            sql_query(
                "ALTER TABLE queued_messages ADD COLUMN user_event_appended INTEGER NOT NULL DEFAULT 0",
            )
            .execute(conn)?;
        }
    }
    sql_query(
        "CREATE INDEX IF NOT EXISTS idx_queued_messages_session ON queued_messages(session_id)",
    )
    .execute(conn)?;
    Ok(())
}

/// Heal DBs that predate `1785657542_repeating_task_tz_and_runs`. The
/// `ALTER TABLE repeating_tasks ADD COLUMN timezone` is the only
/// non-idempotent part; NULL means "UTC" so a healed row behaves
/// exactly like it did before this migration existed.
fn ensure_repeating_tasks_timezone_column(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let rows: Vec<PragmaColumn> = sql_query("PRAGMA table_info(repeating_tasks)").load(conn)?;
    let existing: Vec<String> = rows.into_iter().map(|r| r.name).collect();
    if existing.is_empty() {
        return Ok(());
    }
    if !existing.iter().any(|c| c == "timezone") {
        tracing::info!("Repairing schema: adding repeating_tasks.timezone");
        sql_query("ALTER TABLE repeating_tasks ADD COLUMN timezone TEXT").execute(conn)?;
    }
    Ok(())
}

/// Heal DBs that predate `1785657542_repeating_task_tz_and_runs`:
/// per-dispatch run history for repeating tasks. Also relaxes the
/// `status` CHECK to the post-`1786400000_repeating_task_run_status_widen`
/// shape (adds `corrupt_schedule` / `consumed_once`) — CHECK constraints
/// can only be changed by recreating the table, and SQLite has no
/// IF-CHECK-IS-RELAXED guard, so a detect-then-rebuild heals data dirs
/// that skipped the migration or died halfway through it.
fn ensure_repeating_task_runs_table(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    const CURRENT_STATUS_CHECK: &str = "CHECK (status IN ('spawned', 'already_running', 'throttled', 'failed', 'corrupt_schedule', 'consumed_once'))";

    // Recover from a rebuild (migration or a previous repair pass) that
    // died between DROP and RENAME, or that left its scratch table behind.
    let has_runs = table_exists(conn, "repeating_task_runs")?;
    let has_scratch = table_exists(conn, "repeating_task_runs_new")?;
    if !has_runs && has_scratch {
        tracing::info!("Repairing schema: resuming interrupted repeating_task_runs rebuild");
        sql_query("ALTER TABLE repeating_task_runs_new RENAME TO repeating_task_runs")
            .execute(conn)?;
    } else if has_runs && has_scratch {
        tracing::info!("Repairing schema: dropping stale repeating_task_runs_new scratch table");
        sql_query("DROP TABLE repeating_task_runs_new").execute(conn)?;
    }

    log_if_healing_table(conn, "repeating_task_runs")?;
    sql_query(&format!(
        "CREATE TABLE IF NOT EXISTS repeating_task_runs (
            id          TEXT    PRIMARY KEY NOT NULL,
            task_id     TEXT    NOT NULL REFERENCES repeating_tasks(id),
            session_id  TEXT,
            started_at  TEXT    NOT NULL,
            status      TEXT    NOT NULL {CURRENT_STATUS_CHECK},
            trigger     TEXT    NOT NULL CHECK (trigger IN ('scheduler', 'manual')),
            detail      TEXT
        )"
    ))
    .execute(conn)?;

    #[derive(QueryableByName)]
    struct MasterRow {
        #[diesel(sql_type = diesel::sql_types::Text)]
        sql: String,
    }
    let row: Option<MasterRow> = sql_query(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'repeating_task_runs'",
    )
    .get_result(conn)
    .optional()?;
    // A table whose CHECK doesn't mention the newest status predates the
    // widening migration. Rebuild it, preserving every existing row.
    if row.is_some_and(|r| !r.sql.contains("'consumed_once'")) {
        tracing::info!("Repairing schema: relaxing repeating_task_runs.status CHECK constraint");
        sql_query(&format!(
            "CREATE TABLE repeating_task_runs_new (
                id          TEXT    PRIMARY KEY NOT NULL,
                task_id     TEXT    NOT NULL REFERENCES repeating_tasks(id),
                session_id  TEXT,
                started_at  TEXT    NOT NULL,
                status      TEXT    NOT NULL {CURRENT_STATUS_CHECK},
                trigger     TEXT    NOT NULL CHECK (trigger IN ('scheduler', 'manual')),
                detail      TEXT
            )"
        ))
        .execute(conn)?;
        sql_query(
            "INSERT INTO repeating_task_runs_new (id, task_id, session_id, started_at, status, trigger, detail)
                SELECT id, task_id, session_id, started_at, status, trigger, detail FROM repeating_task_runs",
        )
        .execute(conn)?;
        sql_query("DROP TABLE repeating_task_runs").execute(conn)?;
        sql_query("ALTER TABLE repeating_task_runs_new RENAME TO repeating_task_runs")
            .execute(conn)?;
    }

    sql_query(
        "CREATE INDEX IF NOT EXISTS idx_repeating_task_runs_task ON repeating_task_runs (task_id, started_at)",
    )
    .execute(conn)?;
    Ok(())
}

/// Heal DBs that predate `1784900000_env_vars` / `1785000000_env_var_folder_scope`.
/// Creates the table in its current (post-folder-scope) shape if missing,
/// recovers a DB caught mid-rebuild by the folder-scope migration (which
/// does `CREATE env_vars_new` -> `INSERT ... SELECT` -> `DROP TABLE env_vars`
/// -> `RENAME`), and adds `folder_id` (+ the two partial unique indexes) to
/// any pre-folder-scope table. The legacy table has an inline `UNIQUE(name)`
/// constraint SQLite can't drop with a plain `ALTER TABLE ADD COLUMN`, so a
/// missing `folder_id` is healed the same way the migration does: rebuild.
fn ensure_env_vars_table(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    let has_env_vars = table_exists(conn, "env_vars")?;
    let has_scratch = table_exists(conn, "env_vars_new")?;
    if !has_env_vars && has_scratch {
        tracing::info!("Repairing schema: resuming interrupted env_vars folder-scope rebuild");
        sql_query("ALTER TABLE env_vars_new RENAME TO env_vars").execute(conn)?;
    } else if has_env_vars && has_scratch {
        tracing::info!("Repairing schema: dropping stale env_vars_new scratch table");
        sql_query("DROP TABLE env_vars_new").execute(conn)?;
    }

    log_if_healing_table(conn, "env_vars")?;
    sql_query(
        "CREATE TABLE IF NOT EXISTS env_vars (
            id           TEXT PRIMARY KEY NOT NULL,
            name         TEXT NOT NULL,
            value        TEXT,
            ciphertext   TEXT,
            nonce        TEXT,
            kdf_salt     TEXT,
            encrypted    BOOLEAN NOT NULL DEFAULT 0,
            encrypted_by TEXT,
            folder_id    TEXT,
            created_at   TEXT NOT NULL,
            updated_at   TEXT NOT NULL
        )",
    )
    .execute(conn)?;

    let rows: Vec<PragmaColumn> = sql_query("PRAGMA table_info(env_vars)").load(conn)?;
    let existing: Vec<String> = rows.into_iter().map(|r| r.name).collect();
    if !existing.iter().any(|c| c == "folder_id") {
        tracing::info!("Repairing schema: adding env_vars.folder_id");
        sql_query(
            "CREATE TABLE env_vars_new (
                id           TEXT PRIMARY KEY NOT NULL,
                name         TEXT NOT NULL,
                value        TEXT,
                ciphertext   TEXT,
                nonce        TEXT,
                kdf_salt     TEXT,
                encrypted    BOOLEAN NOT NULL DEFAULT 0,
                encrypted_by TEXT,
                folder_id    TEXT,
                created_at   TEXT NOT NULL,
                updated_at   TEXT NOT NULL
            )",
        )
        .execute(conn)?;
        sql_query(
            "INSERT INTO env_vars_new
                (id, name, value, ciphertext, nonce, kdf_salt, encrypted, encrypted_by, folder_id, created_at, updated_at)
                SELECT id, name, value, ciphertext, nonce, kdf_salt, encrypted, encrypted_by, NULL, created_at, updated_at
                FROM env_vars",
        )
        .execute(conn)?;
        sql_query("DROP TABLE env_vars").execute(conn)?;
        sql_query("ALTER TABLE env_vars_new RENAME TO env_vars").execute(conn)?;
    }

    sql_query(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_env_vars_global_name ON env_vars(name) WHERE folder_id IS NULL",
    )
    .execute(conn)?;
    sql_query(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_env_vars_folder_name ON env_vars(folder_id, name) WHERE folder_id IS NOT NULL",
    )
    .execute(conn)?;
    Ok(())
}

/// Heal DBs that predate `1785000001_agent_vars`.
fn ensure_agent_vars_table(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    log_if_healing_table(conn, "agent_vars")?;
    sql_query(
        "CREATE TABLE IF NOT EXISTS agent_vars (
            id         TEXT PRIMARY KEY NOT NULL,
            name       TEXT NOT NULL,
            value      TEXT NOT NULL,
            folder_id  TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )",
    )
    .execute(conn)?;
    sql_query(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_vars_global_name ON agent_vars(name) WHERE folder_id IS NULL",
    )
    .execute(conn)?;
    sql_query(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_vars_folder_name ON agent_vars(folder_id, name) WHERE folder_id IS NOT NULL",
    )
    .execute(conn)?;
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

    /// A DB created before `1781202566_user_tabs_more_kinds` has the
    /// old CHECK that only allows 'session' / 'project'. ensure_schema
    /// must recreate the table with the relaxed CHECK while preserving
    /// every existing row, AND the recreated table must accept a
    /// 'report' / 'repeating_task' insert.
    #[test]
    fn ensure_schema_relaxes_user_tabs_check_constraint() {
        let mut conn = SqliteConnection::establish(":memory:").unwrap();

        // Other ensure_schema steps stub minimal projects + queued_messages
        // so this test stays scoped to the user_tabs path.
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
        sql_query("CREATE TABLE users (id TEXT PRIMARY KEY NOT NULL)")
            .execute(&mut conn)
            .unwrap();

        // The pre-relaxation user_tabs, byte-for-byte the original CHECK.
        sql_query(
            "CREATE TABLE user_tabs (
                user_id     TEXT NOT NULL REFERENCES users(id),
                item_type   TEXT NOT NULL CHECK (item_type IN ('session', 'project')),
                item_id     TEXT NOT NULL,
                last_active TEXT NOT NULL,
                PRIMARY KEY (user_id, item_type, item_id)
            )",
        )
        .execute(&mut conn)
        .unwrap();
        sql_query("INSERT INTO users (id) VALUES ('u1')")
            .execute(&mut conn)
            .unwrap();
        sql_query(
            "INSERT INTO user_tabs (user_id, item_type, item_id, last_active) \
             VALUES ('u1', 'session', 's1', '2026-06-11T00:00:00Z')",
        )
        .execute(&mut conn)
        .unwrap();

        ensure_schema(&mut conn).unwrap();

        // Existing row preserved.
        let count: CountRow = sql_query(
            "SELECT count(*) AS n FROM user_tabs WHERE user_id = 'u1' AND item_id = 's1'",
        )
        .get_result(&mut conn)
        .unwrap();
        assert_eq!(count.n, 1, "pre-heal session tab row must survive");

        // New kinds now accepted.
        sql_query(
            "INSERT INTO user_tabs (user_id, item_type, item_id, last_active) \
             VALUES ('u1', 'report', '2026-06-11/foo.md', '2026-06-11T00:00:00Z')",
        )
        .execute(&mut conn)
        .expect("report-kind insert should succeed after heal");
        sql_query(
            "INSERT INTO user_tabs (user_id, item_type, item_id, last_active) \
             VALUES ('u1', 'repeating_task', 'rt1', '2026-06-11T00:00:00Z')",
        )
        .execute(&mut conn)
        .expect("repeating_task-kind insert should succeed after heal");

        // Idempotent: second run is a no-op on the relaxed schema.
        ensure_schema(&mut conn).unwrap();
        let count2: CountRow = sql_query("SELECT count(*) AS n FROM user_tabs")
            .get_result(&mut conn)
            .unwrap();
        assert_eq!(count2.n, 3);
    }

    /// A DB created before `1786400000_repeating_task_run_status_widen`
    /// has the narrow `repeating_task_runs.status` CHECK, which rejects
    /// the `corrupt_schedule` / `consumed_once` refusal rows the
    /// dispatcher writes. ensure_schema must rebuild the table with the
    /// relaxed CHECK, preserving existing rows.
    #[test]
    fn ensure_schema_relaxes_repeating_task_runs_status_check() {
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
        sql_query("CREATE TABLE repeating_tasks (id TEXT PRIMARY KEY NOT NULL)")
            .execute(&mut conn)
            .unwrap();
        // The pre-widening table, byte-for-byte the original CHECK.
        sql_query(
            "CREATE TABLE repeating_task_runs (
                id          TEXT    PRIMARY KEY NOT NULL,
                task_id     TEXT    NOT NULL REFERENCES repeating_tasks(id),
                session_id  TEXT,
                started_at  TEXT    NOT NULL,
                status      TEXT    NOT NULL CHECK (status IN ('spawned', 'already_running', 'throttled', 'failed')),
                trigger     TEXT    NOT NULL CHECK (trigger IN ('scheduler', 'manual')),
                detail      TEXT
            )",
        )
        .execute(&mut conn)
        .unwrap();
        sql_query("INSERT INTO repeating_tasks (id) VALUES ('t1')")
            .execute(&mut conn)
            .unwrap();
        sql_query(
            "INSERT INTO repeating_task_runs (id, task_id, started_at, status, trigger) \
             VALUES ('r1', 't1', '2026-08-03T00:00:00Z', 'spawned', 'scheduler')",
        )
        .execute(&mut conn)
        .unwrap();
        // The old CHECK really does reject the refusal statuses.
        assert!(
            sql_query(
                "INSERT INTO repeating_task_runs (id, task_id, started_at, status, trigger) \
                 VALUES ('r0', 't1', '2026-08-03T00:00:00Z', 'corrupt_schedule', 'scheduler')",
            )
            .execute(&mut conn)
            .is_err(),
        );

        ensure_schema(&mut conn).unwrap();

        let count: CountRow =
            sql_query("SELECT count(*) AS n FROM repeating_task_runs WHERE id = 'r1'")
                .get_result(&mut conn)
                .unwrap();
        assert_eq!(count.n, 1, "pre-heal run row must survive");

        for status in ["corrupt_schedule", "consumed_once"] {
            sql_query(&format!(
                "INSERT INTO repeating_task_runs (id, task_id, started_at, status, trigger) \
                 VALUES ('r_{status}', 't1', '2026-08-03T00:00:00Z', '{status}', 'scheduler')"
            ))
            .execute(&mut conn)
            .unwrap_or_else(|e| panic!("{status} insert should succeed after heal: {e}"));
        }

        // Idempotent: second run is a no-op on the relaxed schema.
        ensure_schema(&mut conn).unwrap();
        let count2: CountRow = sql_query("SELECT count(*) AS n FROM repeating_task_runs")
            .get_result(&mut conn)
            .unwrap();
        assert_eq!(count2.n, 3);
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
            "worker_step",
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

    /// A DB that predates `1781053574_projects_workflow_required` has a
    /// `projects` table with the legacy nullable `default_workflow` column
    /// and no `workflow` column. ensure_schema must add the new column with
    /// the NOT NULL constraint, copy each project's `default_workflow` when
    /// set, and fall through to the platform default ('task') otherwise.
    /// The legacy column stays in place — per migration policy we never DROP
    /// in a forward step.
    #[test]
    fn ensure_schema_adds_projects_workflow_with_backfill() {
        let mut conn = SqliteConnection::establish(":memory:").unwrap();
        sql_query(
            "CREATE TABLE projects (
                id TEXT PRIMARY KEY NOT NULL,
                auto_notify_changes BOOLEAN NOT NULL DEFAULT 1,
                worker_communication BOOLEAN NOT NULL DEFAULT 1,
                default_workflow TEXT
            )",
        )
        .execute(&mut conn)
        .unwrap();
        sql_query(
            "INSERT INTO projects (id, default_workflow) VALUES \
             ('p_null', NULL), \
             ('p_empty', ''), \
             ('p_set', 'research')",
        )
        .execute(&mut conn)
        .unwrap();

        ensure_schema(&mut conn).unwrap();

        let cols = project_columns(&mut conn).unwrap();
        assert!(cols.iter().any(|c| c == "workflow"));
        assert!(
            cols.iter().any(|c| c == "default_workflow"),
            "legacy column must survive the heal",
        );

        #[derive(QueryableByName)]
        struct Row {
            #[diesel(sql_type = diesel::sql_types::Text)]
            id: String,
            #[diesel(sql_type = diesel::sql_types::Text)]
            workflow: String,
        }
        let rows: Vec<Row> = sql_query("SELECT id, workflow FROM projects ORDER BY id")
            .load(&mut conn)
            .unwrap();
        let by_id: std::collections::HashMap<_, _> = rows
            .iter()
            .map(|r| (r.id.as_str(), r.workflow.as_str()))
            .collect();
        assert_eq!(by_id["p_null"], "task");
        assert_eq!(by_id["p_empty"], "task");
        assert_eq!(by_id["p_set"], "research");

        // Second call must be a no-op.
        ensure_schema(&mut conn).unwrap();
    }

    /// A DB that predates `1781054693_cards_workflow_required` has a
    /// `cards` table with a nullable `workflow` column. ensure_schema
    /// must rename it to `workflow_legacy`, add a NOT NULL `workflow`
    /// column, and backfill per row: card's own legacy value if set,
    /// then the owning project's workflow, then 'task'.
    #[test]
    fn ensure_schema_adds_cards_workflow_with_per_card_backfill() {
        let mut conn = SqliteConnection::establish(":memory:").unwrap();

        // Minimal projects table — the heal's `(SELECT workflow FROM
        // projects WHERE projects.id = cards.project_id)` subquery
        // needs the table to exist with a workflow column.
        sql_query(
            "CREATE TABLE projects (
                id TEXT PRIMARY KEY NOT NULL,
                auto_notify_changes BOOLEAN NOT NULL DEFAULT 1,
                worker_communication BOOLEAN NOT NULL DEFAULT 1,
                workflow TEXT NOT NULL DEFAULT 'task'
            )",
        )
        .execute(&mut conn)
        .unwrap();
        sql_query(
            "INSERT INTO projects (id, workflow) VALUES \
             ('p_task', 'task'), \
             ('p_research', 'research')",
        )
        .execute(&mut conn)
        .unwrap();

        // Pre-migration `cards` shape with a nullable workflow. `step`,
        // `updated_at`, and `completed_at` exist because the
        // ensure_cards_completed_at_column heal runs first and reads
        // them; we're not testing that path here.
        sql_query(
            "CREATE TABLE cards (
                id TEXT PRIMARY KEY NOT NULL,
                project_id TEXT NOT NULL,
                step TEXT NOT NULL DEFAULT 'backlog',
                workflow TEXT,
                updated_at TEXT NOT NULL DEFAULT '',
                completed_at TEXT
            )",
        )
        .execute(&mut conn)
        .unwrap();
        sql_query(
            "INSERT INTO cards (id, project_id, workflow) VALUES \
             ('c_own', 'p_task', 'breakdown'), \
             ('c_inherit_research', 'p_research', NULL), \
             ('c_inherit_task', 'p_task', NULL), \
             ('c_empty', 'p_research', '')",
        )
        .execute(&mut conn)
        .unwrap();

        ensure_schema(&mut conn).unwrap();

        // Both columns must now exist: new NOT NULL `workflow` and the
        // preserved `workflow_legacy`.
        let rows: Vec<PragmaColumn> = sql_query("PRAGMA table_info(cards)")
            .load(&mut conn)
            .unwrap();
        let names: Vec<String> = rows.into_iter().map(|r| r.name).collect();
        assert!(names.iter().any(|n| n == "workflow"));
        assert!(names.iter().any(|n| n == "workflow_legacy"));

        #[derive(QueryableByName)]
        struct Row {
            #[diesel(sql_type = diesel::sql_types::Text)]
            id: String,
            #[diesel(sql_type = diesel::sql_types::Text)]
            workflow: String,
        }
        let rows: Vec<Row> = sql_query("SELECT id, workflow FROM cards ORDER BY id")
            .load(&mut conn)
            .unwrap();
        let by_id: std::collections::HashMap<_, _> = rows
            .iter()
            .map(|r| (r.id.as_str(), r.workflow.as_str()))
            .collect();
        // Own value wins, regardless of the project's workflow.
        assert_eq!(by_id["c_own"], "breakdown");
        // Null legacy inherits from the project.
        assert_eq!(by_id["c_inherit_research"], "research");
        assert_eq!(by_id["c_inherit_task"], "task");
        // Empty-string legacy is treated the same as null and inherits.
        assert_eq!(by_id["c_empty"], "research");

        // Second call must be a no-op (healthy-path early exit).
        ensure_schema(&mut conn).unwrap();
    }

    fn count(conn: &mut SqliteConnection, where_sql: &str) -> i64 {
        sql_query(format!(
            "SELECT COUNT(*) AS n FROM sessions WHERE {where_sql}"
        ))
        .get_result::<CountRow>(conn)
        .unwrap()
        .n
    }

    #[test]
    fn ensure_sessions_user_id_column_added_when_missing() {
        let mut conn = SqliteConnection::establish(":memory:").unwrap();
        sql_query("CREATE TABLE sessions (id TEXT PRIMARY KEY NOT NULL)")
            .execute(&mut conn)
            .unwrap();
        ensure_sessions_user_id_column(&mut conn).unwrap();
        let cols: Vec<PragmaColumn> = sql_query("PRAGMA table_info(sessions)")
            .load(&mut conn)
            .unwrap();
        assert!(cols.iter().any(|c| c.name == "user_id"));
        // Idempotent second run.
        ensure_sessions_user_id_column(&mut conn).unwrap();
    }

    #[test]
    fn backfill_owns_sessions_for_single_user() {
        let mut conn = SqliteConnection::establish(":memory:").unwrap();
        sql_query("CREATE TABLE users (id TEXT PRIMARY KEY NOT NULL)")
            .execute(&mut conn)
            .unwrap();
        sql_query("CREATE TABLE sessions (id TEXT PRIMARY KEY NOT NULL, user_id TEXT)")
            .execute(&mut conn)
            .unwrap();
        sql_query("INSERT INTO users (id) VALUES ('u1')")
            .execute(&mut conn)
            .unwrap();
        sql_query("INSERT INTO sessions (id, user_id) VALUES ('s1', NULL), ('s2', NULL)")
            .execute(&mut conn)
            .unwrap();
        backfill_session_owners(&mut conn).unwrap();
        assert_eq!(count(&mut conn, "user_id = 'u1'"), 2);
        assert_eq!(count(&mut conn, "user_id IS NULL"), 0);
    }

    #[test]
    fn backfill_leaves_null_for_multiple_users() {
        let mut conn = SqliteConnection::establish(":memory:").unwrap();
        sql_query("CREATE TABLE users (id TEXT PRIMARY KEY NOT NULL)")
            .execute(&mut conn)
            .unwrap();
        sql_query("CREATE TABLE sessions (id TEXT PRIMARY KEY NOT NULL, user_id TEXT)")
            .execute(&mut conn)
            .unwrap();
        sql_query("INSERT INTO users (id) VALUES ('u1'), ('u2')")
            .execute(&mut conn)
            .unwrap();
        sql_query("INSERT INTO sessions (id, user_id) VALUES ('s1', NULL)")
            .execute(&mut conn)
            .unwrap();
        backfill_session_owners(&mut conn).unwrap();
        // Ambiguous ownership -> left NULL per the documented policy.
        assert_eq!(count(&mut conn, "user_id IS NULL"), 1);
    }

    /// A DB that predates `1784200000_session_parent_link` has a `sessions`
    /// table without the subagent link columns, so every
    /// `sessions::table.select(Session::as_select())` dies with "no such
    /// column: parent_session_id". ensure_schema must add both plus the
    /// partial index.
    #[test]
    fn ensure_schema_adds_sessions_parent_link_columns() {
        let mut conn = SqliteConnection::establish(":memory:").unwrap();
        // Other ensure_schema steps prod this table; stub the minimum.
        sql_query(
            "CREATE TABLE projects (
                id TEXT PRIMARY KEY NOT NULL,
                auto_notify_changes BOOLEAN NOT NULL DEFAULT 1,
                worker_communication BOOLEAN NOT NULL DEFAULT 1
            )",
        )
        .execute(&mut conn)
        .unwrap();
        sql_query("CREATE TABLE sessions (id TEXT PRIMARY KEY NOT NULL, name TEXT NOT NULL)")
            .execute(&mut conn)
            .unwrap();
        sql_query("INSERT INTO sessions (id, name) VALUES ('s1', 'Chat')")
            .execute(&mut conn)
            .unwrap();

        ensure_schema(&mut conn).unwrap();

        let cols: Vec<String> = {
            let rows: Vec<PragmaColumn> = sql_query("PRAGMA table_info(sessions)")
                .load(&mut conn)
                .unwrap();
            rows.into_iter().map(|r| r.name).collect()
        };
        for expected in ["parent_session_id", "subagent_completed_at"] {
            assert!(
                cols.iter().any(|c| c == expected),
                "missing {expected}; got {cols:?}",
            );
        }

        let indexes: CountRow = sql_query(
            "SELECT count(*) AS n FROM sqlite_master \
             WHERE type = 'index' AND name = 'idx_sessions_parent_session_id'",
        )
        .get_result(&mut conn)
        .unwrap();
        assert_eq!(indexes.n, 1);

        // The columns are readable, and the pre-existing row survived.
        sql_query("UPDATE sessions SET parent_session_id = 'p1' WHERE id = 's1'")
            .execute(&mut conn)
            .unwrap();
        assert_eq!(count(&mut conn, "parent_session_id = 'p1'"), 1);
        assert_eq!(count(&mut conn, "subagent_completed_at IS NULL"), 1);

        // Second run must be a no-op (no double-add error).
        ensure_schema(&mut conn).unwrap();
    }

    /// A DB that predates `1785200001_doc_review_pr_links` already has
    /// `doc_review_comments` with the original column set, so the
    /// `CREATE TABLE IF NOT EXISTS` heal is a no-op on it. ensure_schema must
    /// still add the two external-link columns (and the pr-links table).
    #[test]
    fn ensure_schema_adds_doc_review_comments_external_columns() {
        let mut conn = SqliteConnection::establish(":memory:").unwrap();
        // Other ensure_schema steps prod this table; stub the minimum.
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
            "CREATE TABLE doc_review_comments (
                id              TEXT    PRIMARY KEY NOT NULL,
                review_id       TEXT    NOT NULL,
                version         INTEGER NOT NULL,
                start_line      INTEGER NOT NULL,
                end_line        INTEGER NOT NULL,
                quote           TEXT,
                kind            TEXT    NOT NULL,
                body            TEXT    NOT NULL,
                status          TEXT    NOT NULL DEFAULT 'pending',
                resolution_note TEXT,
                created_at      TEXT    NOT NULL
            )",
        )
        .execute(&mut conn)
        .unwrap();
        sql_query(
            "INSERT INTO doc_review_comments \
             (id, review_id, version, start_line, end_line, kind, body, created_at) \
             VALUES ('c1', 'r1', 1, 1, 2, 'comment', 'hi', '2026-08-03T00:00:00Z')",
        )
        .execute(&mut conn)
        .unwrap();

        ensure_schema(&mut conn).unwrap();

        let cols: Vec<String> = {
            let rows: Vec<PragmaColumn> = sql_query("PRAGMA table_info(doc_review_comments)")
                .load(&mut conn)
                .unwrap();
            rows.into_iter().map(|r| r.name).collect()
        };
        for expected in ["external_kind", "external_id"] {
            assert!(
                cols.iter().any(|c| c == expected),
                "missing {expected}; got {cols:?}",
            );
        }

        // The imported-annotation read path works on the healed table.
        sql_query(
            "UPDATE doc_review_comments \
             SET external_kind = 'github_pr', external_id = '42' WHERE id = 'c1'",
        )
        .execute(&mut conn)
        .unwrap();
        let hit: CountRow = sql_query(
            "SELECT count(*) AS n FROM doc_review_comments \
             WHERE external_kind = 'github_pr' AND external_id = '42'",
        )
        .get_result(&mut conn)
        .unwrap();
        assert_eq!(hit.n, 1);

        // The link table from the same migration is healed too.
        let tables: CountRow = sql_query(
            "SELECT count(*) AS n FROM sqlite_master \
             WHERE type = 'table' AND name = 'doc_review_pr_links'",
        )
        .get_result(&mut conn)
        .unwrap();
        assert_eq!(tables.n, 1);

        // Second run must be a no-op (no double-add error).
        ensure_schema(&mut conn).unwrap();
    }

    /// Fresh DB (post-migrations state simulated as empty) must get both
    /// tables, `env_vars.folder_id`, and all four partial unique indexes.
    #[test]
    fn ensure_schema_creates_env_and_agent_var_tables() {
        let mut conn = SqliteConnection::establish(":memory:").unwrap();
        // Other ensure_schema steps prod this table; stub the minimum.
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

        for table in ["env_vars", "agent_vars"] {
            assert!(table_exists(&mut conn, table).unwrap(), "missing {table}");
        }
        let env_cols = {
            let rows: Vec<PragmaColumn> = sql_query("PRAGMA table_info(env_vars)")
                .load(&mut conn)
                .unwrap();
            rows.into_iter().map(|r| r.name).collect::<Vec<_>>()
        };
        assert!(env_cols.iter().any(|c| c == "folder_id"));

        for idx in [
            "idx_env_vars_global_name",
            "idx_env_vars_folder_name",
            "idx_agent_vars_global_name",
            "idx_agent_vars_folder_name",
        ] {
            let hit: CountRow = sql_query(format!(
                "SELECT count(*) AS n FROM sqlite_master WHERE type = 'index' AND name = '{idx}'"
            ))
            .get_result(&mut conn)
            .unwrap();
            assert_eq!(hit.n, 1, "missing index {idx}");
        }

        // Second run must be a no-op (no double-add / duplicate-index error).
        ensure_schema(&mut conn).unwrap();
    }

    /// A pre-folder-scope `env_vars` table (inline `UNIQUE(name)`, no
    /// `folder_id`) must be rebuilt with `folder_id` added and the row
    /// preserved.
    #[test]
    fn ensure_schema_adds_env_vars_folder_id() {
        let mut conn = SqliteConnection::establish(":memory:").unwrap();
        // Other ensure_schema steps prod this table; stub the minimum.
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
            "CREATE TABLE env_vars (
                id           TEXT PRIMARY KEY NOT NULL,
                name         TEXT NOT NULL UNIQUE,
                value        TEXT,
                ciphertext   TEXT,
                nonce        TEXT,
                kdf_salt     TEXT,
                encrypted    BOOLEAN NOT NULL DEFAULT 0,
                encrypted_by TEXT,
                created_at   TEXT NOT NULL,
                updated_at   TEXT NOT NULL
            )",
        )
        .execute(&mut conn)
        .unwrap();
        sql_query(
            "INSERT INTO env_vars (id, name, value, encrypted, created_at, updated_at)
                VALUES ('v1', 'API_KEY', 'secret', 0, '2026-01-01', '2026-01-01')",
        )
        .execute(&mut conn)
        .unwrap();

        ensure_schema(&mut conn).unwrap();

        let cols = {
            let rows: Vec<PragmaColumn> = sql_query("PRAGMA table_info(env_vars)")
                .load(&mut conn)
                .unwrap();
            rows.into_iter().map(|r| r.name).collect::<Vec<_>>()
        };
        assert!(
            cols.iter().any(|c| c == "folder_id"),
            "got columns {cols:?}"
        );

        let hit: CountRow = sql_query(
            "SELECT count(*) AS n FROM env_vars WHERE id = 'v1' AND name = 'API_KEY' AND value = 'secret'",
        )
        .get_result(&mut conn)
        .unwrap();
        assert_eq!(hit.n, 1, "row lost during rebuild");

        for idx in ["idx_env_vars_global_name", "idx_env_vars_folder_name"] {
            let hit: CountRow = sql_query(format!(
                "SELECT count(*) AS n FROM sqlite_master WHERE type = 'index' AND name = '{idx}'"
            ))
            .get_result(&mut conn)
            .unwrap();
            assert_eq!(hit.n, 1, "missing index {idx}");
        }

        ensure_schema(&mut conn).unwrap();
    }
}
