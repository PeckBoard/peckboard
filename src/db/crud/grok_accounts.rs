//! CRUD for [`GrokAccount`] — the set of Grok / xAI credentials the spawned
//! `grok` CLI can run as. Mirrors [`super::claude_accounts`]; the "Default"
//! account is implicit (a session whose model id has no `@<account_id>`
//! suffix uses the host's ambient `~/.grok` credentials) and is NOT a row
//! here. Rolling-window usage is read through the provider-agnostic
//! [`Db::account_usage_since`](crate::db::Db::account_usage_since) on the
//! shared `usage_events` table, so there is no Grok-specific usage query.

use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    pub async fn create_grok_account(&self, new: NewGrokAccount) -> anyhow::Result<GrokAccount> {
        self.with_conn(move |conn| {
            diesel::insert_into(grok_accounts::table)
                .values(&new)
                .returning(GrokAccount::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_grok_account(&self, id: &str) -> anyhow::Result<Option<GrokAccount>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            grok_accounts::table
                .find(&id)
                .select(GrokAccount::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_grok_accounts(&self) -> anyhow::Result<Vec<GrokAccount>> {
        self.with_conn(move |conn| {
            grok_accounts::table
                .select(GrokAccount::as_select())
                .order(grok_accounts::created_at.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn update_grok_account(
        &self,
        id: &str,
        changes: GrokAccountChanges,
    ) -> anyhow::Result<Option<GrokAccount>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            diesel::update(grok_accounts::table.find(&id))
                .set(&changes)
                .returning(GrokAccount::as_returning())
                .get_result(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Delete an account and orphan-null its usage rows so historical totals
    /// survive without a dangling pointer. Returns the deleted row's
    /// `config_dir` so the caller can remove the on-disk GROK_HOME. Mirrors
    /// [`Db::delete_claude_account`](crate::db::Db::delete_claude_account).
    pub async fn delete_grok_account(&self, id: &str) -> anyhow::Result<Option<Option<String>>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            conn.transaction(|conn| {
                let dir: Option<Option<String>> = grok_accounts::table
                    .find(&id)
                    .select(grok_accounts::config_dir)
                    .first::<Option<String>>(conn)
                    .optional()?;
                let Some(config_dir) = dir else {
                    return Ok(None);
                };
                diesel::update(usage_events::table.filter(usage_events::account_id.eq(&id)))
                    .set(usage_events::account_id.eq::<Option<String>>(None))
                    .execute(conn)?;
                diesel::delete(grok_accounts::table.find(&id)).execute(conn)?;
                Ok(Some(config_dir))
            })
        })
        .await
    }
}
