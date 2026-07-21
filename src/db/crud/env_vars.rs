use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    /// Every env var, ordered by name for a stable Settings list.
    pub async fn list_env_vars(&self) -> anyhow::Result<Vec<EnvVar>> {
        self.with_conn(move |conn| {
            env_vars::table
                .select(EnvVar::as_select())
                .order(env_vars::name.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Synchronous twin of [`Db::list_env_vars`] for the blocking plugin-host
    /// exec path (runs on a blocking thread; see `Db::with_conn_blocking`).
    pub(crate) fn list_env_vars_blocking(&self) -> anyhow::Result<Vec<EnvVar>> {
        self.with_conn_blocking(|conn| {
            env_vars::table
                .select(EnvVar::as_select())
                .order(env_vars::name.asc())
                .load(conn)
                .map_err(Into::into)
        })
    }

    /// Look up one var by name within a scope (`None` = global).
    pub async fn get_env_var(
        &self,
        name: &str,
        folder_id: Option<&str>,
    ) -> anyhow::Result<Option<EnvVar>> {
        let name = name.to_string();
        let folder_id = folder_id.map(str::to_string);
        self.with_conn(move |conn| {
            let q = env_vars::table
                .filter(env_vars::name.eq(&name))
                .select(EnvVar::as_select());
            match &folder_id {
                Some(fid) => q
                    .filter(env_vars::folder_id.eq(fid))
                    .first(conn)
                    .optional()
                    .map_err(Into::into),
                None => q
                    .filter(env_vars::folder_id.is_null())
                    .first(conn)
                    .optional()
                    .map_err(Into::into),
            }
        })
        .await
    }

    /// Insert a var, or update the existing var with the same
    /// `(name, folder_id)` scope in place (keeping its `id` and
    /// `created_at`). All value/crypto columns are overwritten from `new`,
    /// so flipping a var between plaintext and encrypted clears the fields
    /// that no longer apply.
    pub async fn upsert_env_var(&self, new: NewEnvVar) -> anyhow::Result<EnvVar> {
        self.with_conn(move |conn| {
            let base = env_vars::table
                .filter(env_vars::name.eq(&new.name))
                .select(EnvVar::as_select());
            let existing: Option<EnvVar> = match &new.folder_id {
                Some(fid) => base
                    .filter(env_vars::folder_id.eq(fid))
                    .first(conn)
                    .optional()?,
                None => base
                    .filter(env_vars::folder_id.is_null())
                    .first(conn)
                    .optional()?,
            };
            if let Some(existing) = existing {
                let updated = EnvVar {
                    id: existing.id,
                    name: existing.name,
                    value: new.value,
                    ciphertext: new.ciphertext,
                    nonce: new.nonce,
                    kdf_salt: new.kdf_salt,
                    encrypted: new.encrypted,
                    encrypted_by: new.encrypted_by,
                    folder_id: existing.folder_id,
                    created_at: existing.created_at,
                    updated_at: new.updated_at,
                };
                diesel::update(env_vars::table.find(&updated.id))
                    .set((
                        env_vars::value.eq(&updated.value),
                        env_vars::ciphertext.eq(&updated.ciphertext),
                        env_vars::nonce.eq(&updated.nonce),
                        env_vars::kdf_salt.eq(&updated.kdf_salt),
                        env_vars::encrypted.eq(updated.encrypted),
                        env_vars::encrypted_by.eq(&updated.encrypted_by),
                        env_vars::updated_at.eq(&updated.updated_at),
                    ))
                    .execute(conn)?;
                Ok(updated)
            } else {
                let row = EnvVar {
                    id: new.id.clone(),
                    name: new.name.clone(),
                    value: new.value.clone(),
                    ciphertext: new.ciphertext.clone(),
                    nonce: new.nonce.clone(),
                    kdf_salt: new.kdf_salt.clone(),
                    encrypted: new.encrypted,
                    encrypted_by: new.encrypted_by.clone(),
                    folder_id: new.folder_id.clone(),
                    created_at: new.created_at.clone(),
                    updated_at: new.updated_at.clone(),
                };
                diesel::insert_into(env_vars::table)
                    .values(&new)
                    .execute(conn)?;
                Ok(row)
            }
        })
        .await
    }

    /// Delete a var by id (name alone is only unique per scope).
    /// Idempotent — `false` when nothing was removed.
    pub async fn delete_env_var(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let count = diesel::delete(env_vars::table.find(&id)).execute(conn)?;
            Ok(count > 0)
        })
        .await
    }

    /// Drop every env var scoped to `folder_id` (folder-deletion cleanup).
    pub async fn delete_env_vars_for_folder(&self, folder_id: &str) -> anyhow::Result<usize> {
        let folder_id = folder_id.to_string();
        self.with_conn(move |conn| {
            diesel::delete(env_vars::table.filter(env_vars::folder_id.eq(&folder_id)))
                .execute(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Every encrypted var unlockable by `user_id` (i.e. `encrypted_by` matches),
    /// ordered by name. Used by the re-encrypt-on-password-change path.
    pub async fn list_env_vars_encrypted_by(&self, user_id: &str) -> anyhow::Result<Vec<EnvVar>> {
        let user_id = user_id.to_string();
        self.with_conn(move |conn| {
            env_vars::table
                .filter(env_vars::encrypted.eq(true))
                .filter(env_vars::encrypted_by.eq(&user_id))
                .select(EnvVar::as_select())
                .order(env_vars::name.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Replace the ciphertext/nonce/kdf_salt of an encrypted var by id (used
    /// when re-encrypting under a rotated password). Bumps `updated_at`.
    /// `false` if no row matched.
    pub async fn update_env_var_ciphertext(
        &self,
        id: &str,
        ciphertext: &str,
        nonce: &str,
        kdf_salt: &str,
    ) -> anyhow::Result<bool> {
        let id = id.to_string();
        let ciphertext = ciphertext.to_string();
        let nonce = nonce.to_string();
        let kdf_salt = kdf_salt.to_string();
        let now = chrono::Utc::now().to_rfc3339();
        self.with_conn(move |conn| {
            let count = diesel::update(env_vars::table.find(&id))
                .set((
                    env_vars::ciphertext.eq(&ciphertext),
                    env_vars::nonce.eq(&nonce),
                    env_vars::kdf_salt.eq(&kdf_salt),
                    env_vars::updated_at.eq(&now),
                ))
                .execute(conn)?;
            Ok(count > 0)
        })
        .await
    }
}
