//! CRUD for [`ClaudeAccount`] — the set of Claude/Anthropic credentials
//! the spawned `claude` CLI can run as. The "Default" account is implicit
//! (a session whose model id has no `@<account_id>` suffix uses the host's
//! ambient credentials) and is therefore NOT a row here.

use diesel::prelude::*;
use diesel::sql_types::{BigInt, Nullable, Text};

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

/// One model's token totals for an account over a window. The route layer
/// prices these through [`crate::routes::usage::cost::usage_cost`] so token
/// rates stay in exactly one place. Loaded via raw SQL (like the usage
/// rollups) so `SUM()` is `COALESCE`d to a plain `BIGINT` rather than
/// diesel's `Numeric`.
#[derive(QueryableByName, Debug, Clone)]
pub struct AccountModelUsage {
    #[diesel(sql_type = Nullable<Text>)]
    pub model: Option<String>,
    #[diesel(sql_type = BigInt)]
    pub input_tokens: i64,
    #[diesel(sql_type = BigInt)]
    pub output_tokens: i64,
    #[diesel(sql_type = BigInt)]
    pub cache_read_tokens: i64,
    #[diesel(sql_type = BigInt)]
    pub cache_creation_tokens: i64,
    #[diesel(sql_type = BigInt)]
    pub total_tokens: i64,
    #[diesel(sql_type = BigInt)]
    pub turns: i64,
}

impl Db {
    pub async fn create_claude_account(
        &self,
        new: NewClaudeAccount,
    ) -> anyhow::Result<ClaudeAccount> {
        self.with_conn(move |conn| {
            diesel::insert_into(claude_accounts::table)
                .values(&new)
                .returning(ClaudeAccount::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_claude_account(&self, id: &str) -> anyhow::Result<Option<ClaudeAccount>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            claude_accounts::table
                .find(&id)
                .select(ClaudeAccount::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_claude_accounts(&self) -> anyhow::Result<Vec<ClaudeAccount>> {
        self.with_conn(move |conn| {
            claude_accounts::table
                .select(ClaudeAccount::as_select())
                .order(claude_accounts::created_at.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn update_claude_account(
        &self,
        id: &str,
        changes: ClaudeAccountChanges,
    ) -> anyhow::Result<Option<ClaudeAccount>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            diesel::update(claude_accounts::table.find(&id))
                .set(&changes)
                .returning(ClaudeAccount::as_returning())
                .get_result(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Delete an account and orphan-null its usage rows so historical
    /// totals survive without a dangling pointer (we keep the spend
    /// figures; they just fall back into the "Default / unattributed"
    /// bucket). Returns the deleted row's `config_dir` so the caller can
    /// remove the on-disk CLAUDE_CONFIG_DIR.
    pub async fn delete_claude_account(&self, id: &str) -> anyhow::Result<Option<Option<String>>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            conn.transaction(|conn| {
                let dir: Option<Option<String>> = claude_accounts::table
                    .find(&id)
                    .select(claude_accounts::config_dir)
                    .first::<Option<String>>(conn)
                    .optional()?;
                let Some(config_dir) = dir else {
                    return Ok(None);
                };
                diesel::update(usage_events::table.filter(usage_events::account_id.eq(&id)))
                    .set(usage_events::account_id.eq::<Option<String>>(None))
                    .execute(conn)?;
                diesel::delete(claude_accounts::table.find(&id)).execute(conn)?;
                Ok(Some(config_dir))
            })
        })
        .await
    }

    /// Per-model token sums + turn count an account has billed since
    /// `since_ms`. Drives the rolling-window budget evaluation; the route
    /// prices the rows to USD. `since_ms` of 0 means "all time".
    pub async fn account_usage_since(
        &self,
        account_id: &str,
        since_ms: i64,
    ) -> anyhow::Result<Vec<AccountModelUsage>> {
        let account_id = account_id.to_string();
        self.with_conn(move |conn| {
            diesel::sql_query(
                "SELECT u.model AS model, \
                 COALESCE(SUM(u.input_tokens), 0) AS input_tokens, \
                 COALESCE(SUM(u.output_tokens), 0) AS output_tokens, \
                 COALESCE(SUM(u.cache_read_tokens), 0) AS cache_read_tokens, \
                 COALESCE(SUM(u.cache_creation_tokens), 0) AS cache_creation_tokens, \
                 COALESCE(SUM(u.total_tokens), 0) AS total_tokens, \
                 COUNT(*) AS turns \
                 FROM usage_events u \
                 WHERE u.account_id = ? AND u.ts >= ? \
                 GROUP BY u.model",
            )
            .bind::<Text, _>(account_id)
            .bind::<BigInt, _>(since_ms)
            .load::<AccountModelUsage>(conn)
            .map_err(Into::into)
        })
        .await
    }
}
