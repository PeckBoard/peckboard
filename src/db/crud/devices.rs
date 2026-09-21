use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    /// Insert a newly enrolled device. Fails on a duplicate `secret_hash`
    /// (unique index) — two enrollments can never share a token.
    pub async fn insert_device(&self, new: NewDevice) -> anyhow::Result<Device> {
        self.with_conn(move |conn| {
            diesel::insert_into(devices::table)
                .values(&new)
                .execute(conn)?;
            devices::table
                .find(&new.id)
                .select(Device::as_select())
                .first(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Look up one device by id.
    pub async fn get_device(&self, id: &str) -> anyhow::Result<Option<Device>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            devices::table
                .find(&id)
                .select(Device::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }
    /// Look up one device by its enrollment-token hash (`secret_hash` is
    /// unique). Used by `/ws/agent` auth — the caller hashes the presented
    /// token and never passes the plaintext here.
    pub async fn get_device_by_secret_hash(&self, hash: &str) -> anyhow::Result<Option<Device>> {
        let hash = hash.to_string();
        self.with_conn(move |conn| {
            devices::table
                .filter(devices::secret_hash.eq(&hash))
                .select(Device::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Every device owned by a user, newest first.
    pub async fn list_devices_by_user(&self, user_id: &str) -> anyhow::Result<Vec<Device>> {
        let user_id = user_id.to_string();
        self.with_conn(move |conn| {
            devices::table
                .filter(devices::user_id.eq(&user_id))
                .select(Device::as_select())
                .order(devices::created_at.desc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Set a device's status (`active` / `disabled` / `revoked`). `false`
    /// if the id doesn't exist.
    pub async fn update_device_status(&self, id: &str, status: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        let status = status.to_string();
        self.with_conn(move |conn| {
            let count = diesel::update(devices::table.find(&id))
                .set(devices::status.eq(&status))
                .execute(conn)?;
            Ok(count > 0)
        })
        .await
    }

    /// Rename a device. `false` if the id doesn't exist.
    pub async fn update_device_name(&self, id: &str, name: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        let name = name.to_string();
        self.with_conn(move |conn| {
            let count = diesel::update(devices::table.find(&id))
                .set(devices::name.eq(&name))
                .execute(conn)?;
            Ok(count > 0)
        })
        .await
    }

    /// Stamp `last_seen_at` (RFC3339). `false` if the id doesn't exist.
    pub async fn update_device_last_seen(
        &self,
        id: &str,
        last_seen_at: &str,
    ) -> anyhow::Result<bool> {
        let id = id.to_string();
        let last_seen_at = last_seen_at.to_string();
        self.with_conn(move |conn| {
            let count = diesel::update(devices::table.find(&id))
                .set(devices::last_seen_at.eq(&last_seen_at))
                .execute(conn)?;
            Ok(count > 0)
        })
        .await
    }

    /// Delete a device by id. Idempotent — `false` when nothing was removed.
    pub async fn delete_device(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let count = diesel::delete(devices::table.find(&id)).execute(conn)?;
            Ok(count > 0)
        })
        .await
    }

    /// Append one bridged-action row to a device's audit log.
    pub async fn insert_device_activity(&self, new: NewDeviceActivity) -> anyhow::Result<()> {
        self.with_conn(move |conn| {
            diesel::insert_into(device_activity::table)
                .values(&new)
                .execute(conn)?;
            Ok(())
        })
        .await
    }

    /// A device's audit log, newest first, capped at `limit`.
    pub async fn list_device_activity(
        &self,
        device_id: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<DeviceActivity>> {
        let device_id = device_id.to_string();
        self.with_conn(move |conn| {
            device_activity::table
                .filter(device_activity::device_id.eq(&device_id))
                .select(DeviceActivity::as_select())
                .order((
                    device_activity::created_at.desc(),
                    device_activity::id.desc(),
                ))
                .limit(limit)
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_device(id: &str, user_id: &str, secret_hash: &str) -> NewDevice {
        NewDevice {
            id: id.to_string(),
            user_id: user_id.to_string(),
            name: "laptop".to_string(),
            platform: "linux".to_string(),
            secret_hash: secret_hash.to_string(),
            status: device_status::ACTIVE.to_string(),
            last_seen_at: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[tokio::test]
    async fn insert_lookup_status_last_seen_list_delete() {
        let db = Db::in_memory().unwrap();

        let dev = db
            .insert_device(new_device("d1", "u1", "hash1"))
            .await
            .unwrap();
        assert_eq!(dev.status, device_status::ACTIVE);
        assert_eq!(dev.last_seen_at, None);

        // lookup
        let got = db.get_device("d1").await.unwrap().unwrap();
        assert_eq!(got.id, "d1");
        assert_eq!(got.secret_hash, "hash1");

        // status update
        assert!(
            db.update_device_status("d1", device_status::DISABLED)
                .await
                .unwrap()
        );
        assert_eq!(
            db.get_device("d1").await.unwrap().unwrap().status,
            device_status::DISABLED
        );
        // missing id
        assert!(
            !db.update_device_status("nope", device_status::REVOKED)
                .await
                .unwrap()
        );

        // last_seen update
        assert!(
            db.update_device_last_seen("d1", "2026-02-02T00:00:00Z")
                .await
                .unwrap()
        );
        assert_eq!(
            db.get_device("d1")
                .await
                .unwrap()
                .unwrap()
                .last_seen_at
                .as_deref(),
            Some("2026-02-02T00:00:00Z")
        );

        // list-by-user scoping
        db.insert_device(new_device("d2", "u1", "hash2"))
            .await
            .unwrap();
        db.insert_device(new_device("d3", "u2", "hash3"))
            .await
            .unwrap();
        let u1 = db.list_devices_by_user("u1").await.unwrap();
        assert_eq!(u1.len(), 2);
        assert!(u1.iter().all(|d| d.user_id == "u1"));

        // delete idempotent
        assert!(db.delete_device("d1").await.unwrap());
        assert!(!db.delete_device("d1").await.unwrap());
        assert!(db.get_device("d1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn activity_insert_lists_newest_first_and_scopes_by_device() {
        let db = Db::in_memory().unwrap();
        db.insert_device(new_device("d1", "u1", "hash1"))
            .await
            .unwrap();

        for (i, (dev, status)) in [("d1", "ok"), ("d1", "timeout"), ("d2", "ok")]
            .iter()
            .enumerate()
        {
            db.insert_device_activity(NewDeviceActivity {
                id: format!("a{i}"),
                device_id: dev.to_string(),
                session_id: Some("s1".to_string()),
                capability: "echo".to_string(),
                args_summary: "{\"text\":\"hi\"}".to_string(),
                status: status.to_string(),
                created_at: format!("2026-01-0{}T00:00:00Z", i + 1),
            })
            .await
            .unwrap();
        }

        let rows = db.list_device_activity("d1", 50).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].status, "timeout"); // newest first
        assert_eq!(rows[1].status, "ok");

        let capped = db.list_device_activity("d1", 1).await.unwrap();
        assert_eq!(capped.len(), 1);
    }
}
