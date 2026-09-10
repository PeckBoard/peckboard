//! CRUD for [`CodexAccount`] — the set of ChatGPT / Codex CLI credentials
//! the spawned `codex` CLI can run as. Mirrors [`super::kimi_accounts`]; the
//! "Default" account is implicit (a session whose model id has no
//! `@<account_id>` suffix uses the host's ambient `~/.codex`
//! credentials) and is NOT a row here. Rolling-window usage is read through
//! the provider-agnostic
//! [`Db::account_usage_since`](crate::db::Db::account_usage_since) on the
//! shared `usage_events` table, so there is no Codex-specific usage query.

use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    pub async fn create_codex_account(&self, new: NewCodexAccount) -> anyhow::Result<CodexAccount> {
        self.with_conn(move |conn| {
            diesel::insert_into(codex_accounts::table)
                .values(&new)
                .returning(CodexAccount::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_codex_account(&self, id: &str) -> anyhow::Result<Option<CodexAccount>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            codex_accounts::table
                .find(&id)
                .select(CodexAccount::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_codex_accounts(&self) -> anyhow::Result<Vec<CodexAccount>> {
        self.with_conn(move |conn| {
            codex_accounts::table
                .select(CodexAccount::as_select())
                .order(codex_accounts::created_at.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn update_codex_account(
        &self,
        id: &str,
        changes: CodexAccountChanges,
    ) -> anyhow::Result<Option<CodexAccount>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            diesel::update(codex_accounts::table.find(&id))
                .set(&changes)
                .returning(CodexAccount::as_returning())
                .get_result(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Delete an account and orphan-null its usage rows so historical totals
    /// survive without a dangling pointer. Returns the deleted row's
    /// `config_dir` so the caller can remove the on-disk CODEX_HOME.
    /// Mirrors [`Db::delete_kimi_account`](crate::db::Db::delete_kimi_account).
    pub async fn delete_codex_account(&self, id: &str) -> anyhow::Result<Option<Option<String>>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            conn.transaction(|conn| {
                let dir: Option<Option<String>> = codex_accounts::table
                    .find(&id)
                    .select(codex_accounts::config_dir)
                    .first::<Option<String>>(conn)
                    .optional()?;
                let Some(config_dir) = dir else {
                    return Ok(None);
                };
                diesel::update(usage_events::table.filter(usage_events::account_id.eq(&id)))
                    .set(usage_events::account_id.eq::<Option<String>>(None))
                    .execute(conn)?;
                diesel::delete(codex_accounts::table.find(&id)).execute(conn)?;
                Ok(Some(config_dir))
            })
        })
        .await
    }
}
