use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    /// List every configured registry repository, newest first.
    pub async fn list_plugin_repositories(&self) -> anyhow::Result<Vec<PluginRepositoryRow>> {
        self.with_conn(move |conn| {
            let rows = plugin_repositories::table
                .order(plugin_repositories::added_at.desc())
                .select(PluginRepositoryRow::as_select())
                .load(conn)?;
            Ok(rows)
        })
        .await
    }

    /// Add a registry repository. `url` is the resolved registry.json URL
    /// (the unique key); `label` is the operator's input (slug or URL).
    /// Idempotent — re-adding the same resolved URL refreshes its label.
    pub async fn add_plugin_repository(&self, url: &str, label: &str) -> anyhow::Result<()> {
        let row = PluginRepositoryRow {
            url: url.to_string(),
            label: label.to_string(),
            added_at: chrono::Utc::now().to_rfc3339(),
        };
        self.with_conn(move |conn| {
            diesel::insert_into(plugin_repositories::table)
                .values(&row)
                .on_conflict(plugin_repositories::url)
                .do_update()
                .set(plugin_repositories::label.eq(&row.label))
                .execute(conn)?;
            Ok(())
        })
        .await
    }

    /// Remove a registry repository by its resolved URL. Returns whether a
    /// row was deleted.
    pub async fn remove_plugin_repository(&self, url: &str) -> anyhow::Result<bool> {
        let url = url.to_string();
        self.with_conn(move |conn| {
            let n = diesel::delete(plugin_repositories::table.find(url)).execute(conn)?;
            Ok(n > 0)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn repositories_crud_roundtrip() {
        let db = Db::in_memory().unwrap();
        // Migrations seed the default PeckBoard/plugins repo.
        let seeded = db.list_plugin_repositories().await.unwrap();
        assert_eq!(seeded.len(), 1);
        assert_eq!(seeded[0].label, "PeckBoard/plugins");

        db.add_plugin_repository("https://example.com/registry.json", "example/repo")
            .await
            .unwrap();
        let after = db.list_plugin_repositories().await.unwrap();
        assert_eq!(after.len(), 2);

        // Re-add same url updates the label, doesn't duplicate.
        db.add_plugin_repository("https://example.com/registry.json", "renamed")
            .await
            .unwrap();
        let rows = db.list_plugin_repositories().await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|r| r.label == "renamed"));

        // Remove, and removing the default stays removed.
        assert!(
            db.remove_plugin_repository("https://example.com/registry.json")
                .await
                .unwrap()
        );
        assert!(
            !db.remove_plugin_repository("https://example.com/registry.json")
                .await
                .unwrap()
        );
        assert_eq!(db.list_plugin_repositories().await.unwrap().len(), 1);
    }
}
