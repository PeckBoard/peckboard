use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    /// Every agent var across all scopes, ordered by name for a stable
    /// Settings list.
    pub async fn list_agent_vars(&self) -> anyhow::Result<Vec<AgentVar>> {
        self.with_conn(move |conn| {
            agent_vars::table
                .select(AgentVar::as_select())
                .order(agent_vars::name.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Vars visible to a session in `folder_id`: globals plus that folder's,
    /// ordered by name. Shadowing (folder wins on a name collision) is the
    /// caller's concern.
    pub async fn list_agent_vars_for_folder(
        &self,
        folder_id: &str,
    ) -> anyhow::Result<Vec<AgentVar>> {
        let folder_id = folder_id.to_string();
        self.with_conn(move |conn| {
            agent_vars::table
                .filter(
                    agent_vars::folder_id
                        .is_null()
                        .or(agent_vars::folder_id.eq(&folder_id)),
                )
                .select(AgentVar::as_select())
                .order(agent_vars::name.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Insert a var, or update the existing var with the same
    /// `(name, folder_id)` scope in place (keeping its `id` and
    /// `created_at`).
    pub async fn upsert_agent_var(&self, new: NewAgentVar) -> anyhow::Result<AgentVar> {
        self.with_conn(move |conn| {
            let base = agent_vars::table
                .filter(agent_vars::name.eq(&new.name))
                .select(AgentVar::as_select());
            let existing: Option<AgentVar> = match &new.folder_id {
                Some(fid) => base
                    .filter(agent_vars::folder_id.eq(fid))
                    .first(conn)
                    .optional()?,
                None => base
                    .filter(agent_vars::folder_id.is_null())
                    .first(conn)
                    .optional()?,
            };
            if let Some(existing) = existing {
                let updated = AgentVar {
                    id: existing.id,
                    name: existing.name,
                    value: new.value,
                    folder_id: existing.folder_id,
                    created_at: existing.created_at,
                    updated_at: new.updated_at,
                };
                diesel::update(agent_vars::table.find(&updated.id))
                    .set((
                        agent_vars::value.eq(&updated.value),
                        agent_vars::updated_at.eq(&updated.updated_at),
                    ))
                    .execute(conn)?;
                Ok(updated)
            } else {
                let row = AgentVar {
                    id: new.id.clone(),
                    name: new.name.clone(),
                    value: new.value.clone(),
                    folder_id: new.folder_id.clone(),
                    created_at: new.created_at.clone(),
                    updated_at: new.updated_at.clone(),
                };
                diesel::insert_into(agent_vars::table)
                    .values(&new)
                    .execute(conn)?;
                Ok(row)
            }
        })
        .await
    }

    /// Delete a var by `(name, folder_id)` scope — the MCP `delete_variable`
    /// path. Idempotent — `false` when nothing was removed.
    pub async fn delete_agent_var(
        &self,
        name: &str,
        folder_id: Option<&str>,
    ) -> anyhow::Result<bool> {
        let name = name.to_string();
        let folder_id = folder_id.map(str::to_string);
        self.with_conn(move |conn| {
            let base = diesel::delete(agent_vars::table.filter(agent_vars::name.eq(&name)));
            let count = match &folder_id {
                Some(fid) => base.filter(agent_vars::folder_id.eq(fid)).execute(conn)?,
                None => base.filter(agent_vars::folder_id.is_null()).execute(conn)?,
            };
            Ok(count > 0)
        })
        .await
    }

    /// Delete a var by id — the Settings UI path. Idempotent.
    pub async fn delete_agent_var_by_id(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let count = diesel::delete(agent_vars::table.find(&id)).execute(conn)?;
            Ok(count > 0)
        })
        .await
    }

    /// Drop every agent var scoped to `folder_id` (folder-deletion cleanup).
    pub async fn delete_agent_vars_for_folder(&self, folder_id: &str) -> anyhow::Result<usize> {
        let folder_id = folder_id.to_string();
        self.with_conn(move |conn| {
            diesel::delete(agent_vars::table.filter(agent_vars::folder_id.eq(&folder_id)))
                .execute(conn)
                .map_err(Into::into)
        })
        .await
    }
}
