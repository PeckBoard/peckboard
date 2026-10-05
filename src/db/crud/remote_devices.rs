use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    /// Insert a newly paired remote-access device.
    pub async fn insert_remote_device(&self, new: NewRemoteDevice) -> anyhow::Result<RemoteDevice> {
        self.with_conn(move |conn| {
            diesel::insert_into(remote_devices::table)
                .values(&new)
                .execute(conn)?;
            remote_devices::table
                .find(&new.id)
                .select(RemoteDevice::as_select())
                .first(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Look up one paired device by id.
    pub async fn get_remote_device(&self, id: &str) -> anyhow::Result<Option<RemoteDevice>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            remote_devices::table
                .find(&id)
                .select(RemoteDevice::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Every paired device, newest first.
    pub async fn list_remote_devices(&self) -> anyhow::Result<Vec<RemoteDevice>> {
        self.with_conn(move |conn| {
            remote_devices::table
                .select(RemoteDevice::as_select())
                .order(remote_devices::created_at.desc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Rename a paired device. `false` if the id doesn't exist.
    pub async fn rename_remote_device(&self, id: &str, name: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        let name = name.to_string();
        self.with_conn(move |conn| {
            let count = diesel::update(remote_devices::table.find(&id))
                .set(remote_devices::name.eq(&name))
                .execute(conn)?;
            Ok(count > 0)
        })
        .await
    }

    /// Stamp `last_connected_at` (RFC3339). `false` if the id doesn't exist.
    pub async fn touch_remote_device(&self, id: &str, at: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        let at = at.to_string();
        self.with_conn(move |conn| {
            let count = diesel::update(remote_devices::table.find(&id))
                .set(remote_devices::last_connected_at.eq(&at))
                .execute(conn)?;
            Ok(count > 0)
        })
        .await
    }

    /// Delete a paired device — and with it its sealed pairing secret and
    /// its pairing-v2 enrollment row (the FK cascades too; deleting it
    /// here keeps revoke correct even on a connection without
    /// `foreign_keys`). Idempotent: `false` when nothing was removed.
    pub async fn delete_remote_device(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                diesel::delete(remote_device_enrollments::table.find(&id)).execute(conn)?;
                let count = diesel::delete(remote_devices::table.find(&id)).execute(conn)?;
                Ok(count > 0)
            })
        })
        .await
    }
}
