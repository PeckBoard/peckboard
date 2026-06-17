use std::collections::HashMap;

use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

/// Load every stored setting for `plugin_id` as a `key → json_value` map.
/// Shared by the async [`Db::list_plugin_settings`] and the synchronous
/// [`Db::list_plugin_settings_blocking`] so both go through identical SQL.
fn list_plugin_settings_query(
    conn: &mut SqliteConnection,
    plugin_id: &str,
) -> anyhow::Result<HashMap<String, serde_json::Value>> {
    let rows: Vec<PluginSettingRow> = plugin_settings::table
        .filter(plugin_settings::plugin_id.eq(plugin_id))
        .select(PluginSettingRow::as_select())
        .load(conn)?;
    let mut out = HashMap::with_capacity(rows.len());
    for row in rows {
        // Stored values are always JSON-encoded by `set_plugin_setting`.
        // A row whose JSON has rotted (manual edit, corruption) is
        // surfaced as `Value::Null` rather than crashing the page —
        // the caller can fall back to the schema's default.
        let value = serde_json::from_str(&row.value).unwrap_or(serde_json::Value::Null);
        out.insert(row.key, value);
    }
    Ok(out)
}

/// Read a single stored setting for `plugin_id`, or `None` if unset.
/// A row whose JSON has rotted comes back as `Some(Value::Null)`.
fn get_plugin_setting_query(
    conn: &mut SqliteConnection,
    plugin_id: &str,
    key: &str,
) -> anyhow::Result<Option<serde_json::Value>> {
    let raw: Option<String> = plugin_settings::table
        .find((plugin_id, key))
        .select(plugin_settings::value)
        .first(conn)
        .optional()?;
    Ok(raw.map(|v| serde_json::from_str(&v).unwrap_or(serde_json::Value::Null)))
}

/// Atomically apply N upserts/deletes for `plugin_id` inside one transaction.
/// A `Value::Null` deletes the row. Shared by the async and blocking batch
/// writers so both honour the same all-or-nothing semantics.
fn apply_plugin_settings_batch(
    conn: &mut SqliteConnection,
    plugin_id: &str,
    now: &str,
    updates: Vec<(String, serde_json::Value)>,
) -> anyhow::Result<()> {
    conn.transaction::<_, anyhow::Error, _>(|conn| {
        for (key, value) in updates {
            if value.is_null() {
                diesel::delete(plugin_settings::table.find((plugin_id, &key))).execute(conn)?;
                continue;
            }
            let row = PluginSettingRow {
                plugin_id: plugin_id.to_string(),
                key: key.clone(),
                value: serde_json::to_string(&value)?,
                updated_at: now.to_string(),
            };
            diesel::insert_into(plugin_settings::table)
                .values(&row)
                .on_conflict((plugin_settings::plugin_id, plugin_settings::key))
                .do_update()
                .set((
                    plugin_settings::value.eq(&row.value),
                    plugin_settings::updated_at.eq(&row.updated_at),
                ))
                .execute(conn)?;
        }
        Ok(())
    })
}

impl Db {
    /// Load every stored setting for `plugin_id` as a `key → json_value` map.
    /// Values come back as `serde_json::Value` so callers can pattern-match
    /// against the field's declared type without re-parsing.
    pub async fn list_plugin_settings(
        &self,
        plugin_id: &str,
    ) -> anyhow::Result<HashMap<String, serde_json::Value>> {
        let plugin_id = plugin_id.to_string();
        self.with_conn(move |conn| list_plugin_settings_query(conn, &plugin_id))
            .await
    }

    /// Synchronous twin of [`Db::list_plugin_settings`] for plugin host
    /// functions (`src/plugin/host.rs`), which run inside a synchronous
    /// extism call and cannot enter the async runtime.
    pub(crate) fn list_plugin_settings_blocking(
        &self,
        plugin_id: &str,
    ) -> anyhow::Result<HashMap<String, serde_json::Value>> {
        let plugin_id = plugin_id.to_string();
        self.with_conn_blocking(move |conn| list_plugin_settings_query(conn, &plugin_id))
    }

    /// Synchronous read of a single setting for plugin host functions.
    /// Returns `None` when the key is unset.
    pub(crate) fn get_plugin_setting_blocking(
        &self,
        plugin_id: &str,
        key: &str,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        let plugin_id = plugin_id.to_string();
        let key = key.to_string();
        self.with_conn_blocking(move |conn| get_plugin_setting_query(conn, &plugin_id, &key))
    }

    /// Upsert a single setting. Empty `Value::Null` deletes the row so the
    /// schema's default takes over — matches the workflow-instructions
    /// convention.
    pub async fn set_plugin_setting(
        &self,
        plugin_id: &str,
        key: &str,
        value: &serde_json::Value,
    ) -> anyhow::Result<()> {
        self.set_plugin_settings_batch(plugin_id, vec![(key.to_string(), value.clone())])
            .await
    }

    /// Synchronous twin of [`Db::set_plugin_setting`] for plugin host
    /// functions. A `Value::Null` deletes the key.
    pub(crate) fn set_plugin_setting_blocking(
        &self,
        plugin_id: &str,
        key: &str,
        value: &serde_json::Value,
    ) -> anyhow::Result<()> {
        let plugin_id = plugin_id.to_string();
        let now = chrono::Utc::now().to_rfc3339();
        let updates = vec![(key.to_string(), value.clone())];
        self.with_conn_blocking(move |conn| {
            apply_plugin_settings_batch(conn, &plugin_id, &now, updates)
        })
    }

    /// Atomically apply N upserts/deletes inside one transaction.
    /// Either every row is written or none — matters for the settings
    /// API, where a half-applied save would leave the plugin in a
    /// surprising mid-config state. Also a single SQLite round-trip,
    /// where the per-call variant would do N.
    pub async fn set_plugin_settings_batch(
        &self,
        plugin_id: &str,
        updates: Vec<(String, serde_json::Value)>,
    ) -> anyhow::Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        let plugin_id = plugin_id.to_string();
        let now = chrono::Utc::now().to_rfc3339();
        self.with_conn(move |conn| apply_plugin_settings_batch(conn, &plugin_id, &now, updates))
            .await
    }

    /// Delete every stored setting for `plugin_id`. Used when a plugin is
    /// uninstalled so a later reinstall of the same id starts from the
    /// schema defaults instead of its old, possibly stale, values.
    pub async fn delete_plugin_settings(&self, plugin_id: &str) -> anyhow::Result<()> {
        let plugin_id = plugin_id.to_string();
        self.with_conn(move |conn| {
            diesel::delete(
                plugin_settings::table.filter(plugin_settings::plugin_id.eq(&plugin_id)),
            )
            .execute(conn)?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use crate::db::Db;

    #[tokio::test]
    async fn delete_plugin_settings_clears_only_that_plugin() {
        let db = Db::in_memory().unwrap();
        db.set_plugin_setting("a", "k1", &serde_json::json!("v1"))
            .await
            .unwrap();
        db.set_plugin_setting("a", "k2", &serde_json::json!("v2"))
            .await
            .unwrap();
        db.set_plugin_setting("b", "k", &serde_json::json!("keep"))
            .await
            .unwrap();

        // Removing plugin `a`'s settings drops both its rows...
        db.delete_plugin_settings("a").await.unwrap();
        assert!(db.list_plugin_settings("a").await.unwrap().is_empty());
        // ...but leaves an unrelated plugin's settings untouched.
        assert_eq!(db.list_plugin_settings("b").await.unwrap().len(), 1);

        // Deleting again (no rows) is a no-op.
        db.delete_plugin_settings("a").await.unwrap();
    }
}
