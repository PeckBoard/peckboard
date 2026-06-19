//! Generic, plugin-owned storage: a document store (`plugin_data`) and
//! per-session plugin metadata (`plugin_session_meta`). Both are namespaced
//! by plugin id and hold opaque per-plugin JSON core never queries into.
//!
//! Only the synchronous `*_blocking` twins exist for now — these back the
//! `data_store` / `session_read` / `session_write` host functions in
//! `src/plugin/host.rs`, which run inside a synchronous extism call. Add
//! async twins (via `with_conn`) if a route ever needs them.

use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::{PluginDataRow, PluginSessionMetaRow};
use crate::db::schema::*;

impl Db {
    /// Upsert one document into a plugin's store. `data` is stored verbatim
    /// (already JSON-encoded by the caller). `created_at` is preserved across
    /// updates; `updated_at` advances.
    pub(crate) fn plugin_store_put_blocking(
        &self,
        plugin_id: &str,
        collection: &str,
        key: &str,
        data: &str,
    ) -> anyhow::Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let row = PluginDataRow {
            plugin_id: plugin_id.to_string(),
            collection: collection.to_string(),
            key: key.to_string(),
            data: data.to_string(),
            created_at: now.clone(),
            updated_at: now.clone(),
        };
        self.with_conn_blocking(move |conn| {
            diesel::insert_into(plugin_data::table)
                .values(&row)
                .on_conflict((
                    plugin_data::plugin_id,
                    plugin_data::collection,
                    plugin_data::key,
                ))
                .do_update()
                .set((
                    plugin_data::data.eq(&row.data),
                    plugin_data::updated_at.eq(&row.updated_at),
                ))
                .execute(conn)?;
            Ok(())
        })
    }

    /// Read one document's raw JSON, or `None` if absent.
    pub(crate) fn plugin_store_get_blocking(
        &self,
        plugin_id: &str,
        collection: &str,
        key: &str,
    ) -> anyhow::Result<Option<String>> {
        let (plugin_id, collection, key) = (
            plugin_id.to_string(),
            collection.to_string(),
            key.to_string(),
        );
        self.with_conn_blocking(move |conn| {
            let raw: Option<String> = plugin_data::table
                .find((&plugin_id, &collection, &key))
                .select(plugin_data::data)
                .first(conn)
                .optional()?;
            Ok(raw)
        })
    }

    /// List every `(key, raw_json)` in a plugin's collection, key-ordered.
    pub(crate) fn plugin_store_list_blocking(
        &self,
        plugin_id: &str,
        collection: &str,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let (plugin_id, collection) = (plugin_id.to_string(), collection.to_string());
        self.with_conn_blocking(move |conn| {
            let rows: Vec<(String, String)> = plugin_data::table
                .filter(plugin_data::plugin_id.eq(&plugin_id))
                .filter(plugin_data::collection.eq(&collection))
                .order(plugin_data::key.asc())
                .select((plugin_data::key, plugin_data::data))
                .load(conn)?;
            Ok(rows)
        })
    }

    /// Delete one document. Missing key is a no-op (returns `false`).
    pub(crate) fn plugin_store_delete_blocking(
        &self,
        plugin_id: &str,
        collection: &str,
        key: &str,
    ) -> anyhow::Result<bool> {
        let (plugin_id, collection, key) = (
            plugin_id.to_string(),
            collection.to_string(),
            key.to_string(),
        );
        self.with_conn_blocking(move |conn| {
            let n = diesel::delete(plugin_data::table.find((&plugin_id, &collection, &key)))
                .execute(conn)?;
            Ok(n > 0)
        })
    }

    /// Upsert a plugin's metadata blob for a session (`data` is raw JSON).
    pub(crate) fn plugin_session_meta_set_blocking(
        &self,
        session_id: &str,
        plugin_id: &str,
        data: &str,
    ) -> anyhow::Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let row = PluginSessionMetaRow {
            session_id: session_id.to_string(),
            plugin_id: plugin_id.to_string(),
            data: data.to_string(),
            updated_at: now,
        };
        self.with_conn_blocking(move |conn| {
            diesel::insert_into(plugin_session_meta::table)
                .values(&row)
                .on_conflict((
                    plugin_session_meta::session_id,
                    plugin_session_meta::plugin_id,
                ))
                .do_update()
                .set((
                    plugin_session_meta::data.eq(&row.data),
                    plugin_session_meta::updated_at.eq(&row.updated_at),
                ))
                .execute(conn)?;
            Ok(())
        })
    }

    /// Read a plugin's metadata blob for a session, or `None`.
    pub(crate) fn plugin_session_meta_get_blocking(
        &self,
        session_id: &str,
        plugin_id: &str,
    ) -> anyhow::Result<Option<String>> {
        let (session_id, plugin_id) = (session_id.to_string(), plugin_id.to_string());
        self.with_conn_blocking(move |conn| {
            let raw: Option<String> = plugin_session_meta::table
                .find((&session_id, &plugin_id))
                .select(plugin_session_meta::data)
                .first(conn)
                .optional()?;
            Ok(raw)
        })
    }

    /// List every `(session_id, raw_json)` metadata row owned by a plugin.
    pub(crate) fn plugin_session_meta_list_blocking(
        &self,
        plugin_id: &str,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let plugin_id = plugin_id.to_string();
        self.with_conn_blocking(move |conn| {
            let rows: Vec<(String, String)> = plugin_session_meta::table
                .filter(plugin_session_meta::plugin_id.eq(&plugin_id))
                .select((plugin_session_meta::session_id, plugin_session_meta::data))
                .load(conn)?;
            Ok(rows)
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::db::Db;

    #[test]
    fn store_put_get_list_delete_roundtrip_and_is_plugin_scoped() {
        let db = Db::in_memory().unwrap();
        db.plugin_store_put_blocking("exp--rt", "decisions", "d1", r#"{"q":"a"}"#)
            .unwrap();
        db.plugin_store_put_blocking("exp--rt", "decisions", "d2", r#"{"q":"b"}"#)
            .unwrap();
        // A different plugin's identical keys are isolated.
        db.plugin_store_put_blocking("other", "decisions", "d1", r#"{"x":1}"#)
            .unwrap();

        assert_eq!(
            db.plugin_store_get_blocking("exp--rt", "decisions", "d1")
                .unwrap()
                .as_deref(),
            Some(r#"{"q":"a"}"#)
        );
        let list = db
            .plugin_store_list_blocking("exp--rt", "decisions")
            .unwrap();
        assert_eq!(list.len(), 2, "only this plugin's rows");
        assert_eq!(list[0].0, "d1");

        assert!(
            db.plugin_store_delete_blocking("exp--rt", "decisions", "d1")
                .unwrap()
        );
        assert!(
            !db.plugin_store_delete_blocking("exp--rt", "decisions", "d1")
                .unwrap(),
            "second delete is a no-op"
        );
        // The other plugin's row is untouched.
        assert!(
            db.plugin_store_get_blocking("other", "decisions", "d1")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn session_meta_upsert_roundtrip() {
        let db = Db::in_memory().unwrap();
        db.plugin_session_meta_set_blocking("sess1", "experts", r#"{"kind":"pm"}"#)
            .unwrap();
        assert_eq!(
            db.plugin_session_meta_get_blocking("sess1", "experts")
                .unwrap()
                .as_deref(),
            Some(r#"{"kind":"pm"}"#)
        );
        // Upsert overwrites.
        db.plugin_session_meta_set_blocking("sess1", "experts", r#"{"kind":"knowledge"}"#)
            .unwrap();
        assert_eq!(
            db.plugin_session_meta_get_blocking("sess1", "experts")
                .unwrap()
                .as_deref(),
            Some(r#"{"kind":"knowledge"}"#)
        );
        assert!(
            db.plugin_session_meta_get_blocking("sess1", "nobody")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn session_meta_list_is_plugin_scoped() {
        let db = Db::in_memory().unwrap();
        db.plugin_session_meta_set_blocking("sessA", "experts", r#"{"k":1}"#)
            .unwrap();
        db.plugin_session_meta_set_blocking("sessB", "experts", r#"{"k":2}"#)
            .unwrap();
        // A different plugin's rows must not leak in.
        db.plugin_session_meta_set_blocking("sessA", "other", r#"{"x":9}"#)
            .unwrap();

        let mut rows = db.plugin_session_meta_list_blocking("experts").unwrap();
        rows.sort();
        assert_eq!(
            rows,
            vec![
                ("sessA".to_string(), r#"{"k":1}"#.to_string()),
                ("sessB".to_string(), r#"{"k":2}"#.to_string()),
            ]
        );

        let other = db.plugin_session_meta_list_blocking("other").unwrap();
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].0, "sessA");
    }
}
