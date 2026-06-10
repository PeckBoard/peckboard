use std::collections::HashMap;

use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    /// Load every stored setting for `plugin_id` as a `key → json_value` map.
    /// Values come back as `serde_json::Value` so callers can pattern-match
    /// against the field's declared type without re-parsing.
    pub async fn list_plugin_settings(
        &self,
        plugin_id: &str,
    ) -> anyhow::Result<HashMap<String, serde_json::Value>> {
        let plugin_id = plugin_id.to_string();
        self.with_conn(move |conn| {
            let rows: Vec<PluginSettingRow> = plugin_settings::table
                .filter(plugin_settings::plugin_id.eq(&plugin_id))
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
        })
        .await
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
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                for (key, value) in updates {
                    if value.is_null() {
                        diesel::delete(plugin_settings::table.find((&plugin_id, &key)))
                            .execute(conn)?;
                        continue;
                    }
                    let row = PluginSettingRow {
                        plugin_id: plugin_id.clone(),
                        key: key.clone(),
                        value: serde_json::to_string(&value)?,
                        updated_at: now.clone(),
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
        })
        .await
    }
}
