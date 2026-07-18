//! CRUD for [`KimiAccount`] — the set of Moonshot AI / Kimi Code credentials
//! the spawned `kimi` CLI can run as. Mirrors [`super::grok_accounts`]; the
//! "Default" account is implicit (a session whose model id has no
//! `@<account_id>` suffix uses the host's ambient `~/.kimi-code`
//! credentials) and is NOT a row here. Rolling-window usage is read through
//! the provider-agnostic
//! [`Db::account_usage_since`](crate::db::Db::account_usage_since) on the
//! shared `usage_events` table, so there is no Kimi-specific usage query.

use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    pub async fn create_kimi_account(&self, new: NewKimiAccount) -> anyhow::Result<KimiAccount> {
        self.with_conn(move |conn| {
            diesel::insert_into(kimi_accounts::table)
                .values(&new)
                .returning(KimiAccount::as_returning())
                .get_result(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_kimi_account(&self, id: &str) -> anyhow::Result<Option<KimiAccount>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            kimi_accounts::table
                .find(&id)
                .select(KimiAccount::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub async fn list_kimi_accounts(&self) -> anyhow::Result<Vec<KimiAccount>> {
        self.with_conn(move |conn| {
            kimi_accounts::table
                .select(KimiAccount::as_select())
                .order(kimi_accounts::created_at.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn update_kimi_account(
        &self,
        id: &str,
        changes: KimiAccountChanges,
    ) -> anyhow::Result<Option<KimiAccount>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            diesel::update(kimi_accounts::table.find(&id))
                .set(&changes)
                .returning(KimiAccount::as_returning())
                .get_result(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Delete an account and orphan-null its usage rows so historical totals
    /// survive without a dangling pointer. Returns the deleted row's
    /// `config_dir` so the caller can remove the on-disk KIMI_CODE_HOME.
    /// Mirrors [`Db::delete_grok_account`](crate::db::Db::delete_grok_account).
    pub async fn delete_kimi_account(&self, id: &str) -> anyhow::Result<Option<Option<String>>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            conn.transaction(|conn| {
                let dir: Option<Option<String>> = kimi_accounts::table
                    .find(&id)
                    .select(kimi_accounts::config_dir)
                    .first::<Option<String>>(conn)
                    .optional()?;
                let Some(config_dir) = dir else {
                    return Ok(None);
                };
                diesel::update(usage_events::table.filter(usage_events::account_id.eq(&id)))
                    .set(usage_events::account_id.eq::<Option<String>>(None))
                    .execute(conn)?;
                diesel::delete(kimi_accounts::table.find(&id)).execute(conn)?;
                Ok(Some(config_dir))
            })
        })
        .await
    }
}
