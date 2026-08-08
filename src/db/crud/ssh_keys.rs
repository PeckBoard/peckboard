use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    /// Every key, ordered by name for a stable Settings list.
    pub async fn list_ssh_keys(&self) -> anyhow::Result<Vec<SshKey>> {
        self.with_conn(move |conn| {
            ssh_keys::table
                .select(SshKey::as_select())
                .order(ssh_keys::name.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Look up one key by id.
    pub async fn get_ssh_key(&self, id: &str) -> anyhow::Result<Option<SshKey>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            ssh_keys::table
                .find(&id)
                .select(SshKey::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Look up one key by its unique name.
    pub async fn get_ssh_key_by_name(&self, name: &str) -> anyhow::Result<Option<SshKey>> {
        let name = name.to_string();
        self.with_conn(move |conn| {
            ssh_keys::table
                .filter(ssh_keys::name.eq(&name))
                .select(SshKey::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Insert a newly imported/generated key. Fails on a duplicate name
    /// (unique index) — callers should check `get_ssh_key_by_name` first
    /// for a friendlier error, but the DB constraint is the real guard.
    pub async fn insert_ssh_key(&self, new: NewSshKey) -> anyhow::Result<SshKey> {
        self.with_conn(move |conn| {
            diesel::insert_into(ssh_keys::table)
                .values(&new)
                .execute(conn)?;
            ssh_keys::table
                .find(&new.id)
                .select(SshKey::as_select())
                .first(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Rename a key by id. `false` if the id doesn't exist.
    pub async fn rename_ssh_key(&self, id: &str, name: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        let changes = SshKeyRename {
            name: name.to_string(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        };
        self.with_conn(move |conn| {
            let count = diesel::update(ssh_keys::table.find(&id))
                .set(&changes)
                .execute(conn)?;
            Ok(count > 0)
        })
        .await
    }

    /// Delete a key by id. Idempotent — `false` when nothing was removed.
    pub async fn delete_ssh_key(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let count = diesel::delete(ssh_keys::table.find(&id)).execute(conn)?;
            Ok(count > 0)
        })
        .await
    }
}
