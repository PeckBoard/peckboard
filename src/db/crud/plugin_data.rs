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
    /// updates; `updated_at` advances. Public so integration tests can seed
    /// app settings (`core.settings`) the way the settings routes do.
    pub fn plugin_store_put_blocking(
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
    /// Insert one document only if its key is absent — or if the existing
    /// row is older than `ttl_secs` (0 = never steal). Runs as one blocking
    /// DB call on the shared connection, so concurrent callers serialize and
    /// exactly one acquires. This is the primitive behind the
    /// `peckboard_store_put_if_absent` host function — the cross-instance
    /// mutex/lease for plugins that opt in to manifest `concurrency`, whose
    /// pooled instances share no guest memory to lock with. The TTL exists
    /// so a holder that dies mid-critical-section (trap, timeout) leaks the
    /// lease for at most `ttl_secs`.
    pub(crate) fn plugin_store_put_if_absent_blocking(
        &self,
        plugin_id: &str,
        collection: &str,
        key: &str,
        data: &str,
        ttl_secs: u64,
    ) -> anyhow::Result<bool> {
        let now = chrono::Utc::now();
        let row = PluginDataRow {
            plugin_id: plugin_id.to_string(),
            collection: collection.to_string(),
            key: key.to_string(),
            data: data.to_string(),
            created_at: now.to_rfc3339(),
            updated_at: now.to_rfc3339(),
        };
        self.with_conn_blocking(move |conn| {
            let inserted = diesel::insert_into(plugin_data::table)
                .values(&row)
                .on_conflict((
                    plugin_data::plugin_id,
                    plugin_data::collection,
                    plugin_data::key,
                ))
                .do_nothing()
                .execute(conn)?;
            if inserted > 0 {
                return Ok(true);
            }
            if ttl_secs == 0 {
                return Ok(false);
            }
            // Steal only a stale row. `to_rfc3339` always renders UTC with
            // the same offset format, so the lexicographic comparison is a
            // chronological one.
            let cutoff = (now - chrono::Duration::seconds(ttl_secs.min(i64::MAX as u64) as i64))
                .to_rfc3339();
            let n = diesel::update(
                plugin_data::table
                    .find((&row.plugin_id, &row.collection, &row.key))
                    .filter(plugin_data::updated_at.lt(&cutoff)),
            )
            .set((
                plugin_data::data.eq(&row.data),
                plugin_data::updated_at.eq(&row.updated_at),
            ))
            .execute(conn)?;
            Ok(n > 0)
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
    fn store_put_if_absent_acquires_once_and_steals_only_stale() {
        let db = Db::in_memory().unwrap();
        assert!(
            db.plugin_store_put_if_absent_blocking("p", "locks", "orch-1", r#"{"t":1}"#, 60)
                .unwrap(),
            "absent key acquires"
        );
        assert!(
            !db.plugin_store_put_if_absent_blocking("p", "locks", "orch-1", r#"{"t":2}"#, 60)
                .unwrap(),
            "fresh holder is not displaced"
        );
        assert!(
            !db.plugin_store_put_if_absent_blocking("p", "locks", "orch-1", r#"{"t":3}"#, 0)
                .unwrap(),
            "ttl 0 never steals"
        );
        // The winner's payload survives the losing attempts.
        assert_eq!(
            db.plugin_store_get_blocking("p", "locks", "orch-1")
                .unwrap()
                .as_deref(),
            Some(r#"{"t":1}"#)
        );
        // A stale row (holder died) is stolen once its ttl passes — backdate
        // the row instead of sleeping.
        {
            use diesel::prelude::*;
            let old = (chrono::Utc::now() - chrono::Duration::seconds(120)).to_rfc3339();
            db.with_conn_blocking(move |conn| {
                diesel::update(
                    crate::db::schema::plugin_data::table.find(("p", "locks", "orch-1")),
                )
                .set(crate::db::schema::plugin_data::updated_at.eq(&old))
                .execute(conn)?;
                Ok(())
            })
            .unwrap();
        }
        assert!(
            db.plugin_store_put_if_absent_blocking("p", "locks", "orch-1", r#"{"t":4}"#, 60)
                .unwrap(),
            "stale holder is stolen"
        );
        // Released (deleted) locks acquire cleanly again.
        assert!(
            db.plugin_store_delete_blocking("p", "locks", "orch-1")
                .unwrap()
        );
        assert!(
            db.plugin_store_put_if_absent_blocking("p", "locks", "orch-1", r#"{"t":5}"#, 60)
                .unwrap()
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
