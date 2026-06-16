use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

/// `status` value for an approved plugin.
pub const APPROVAL_APPROVED: &str = "approved";
/// `status` value for a denied plugin.
pub const APPROVAL_DENIED: &str = "denied";

fn get_plugin_approval_query(
    conn: &mut SqliteConnection,
    plugin_id: &str,
) -> anyhow::Result<Option<PluginApprovalRow>> {
    let row = plugin_approvals::table
        .find(plugin_id)
        .select(PluginApprovalRow::as_select())
        .first(conn)
        .optional()?;
    Ok(row)
}

impl Db {
    /// Read the stored approval decision for `plugin_id`, or `None` if the
    /// operator has never decided on it. Blocking because plugins load
    /// synchronously (`PluginManager::load_plugin`), outside the async
    /// runtime — same reason the host functions use blocking reads.
    ///
    /// The caller compares the row's `hooks` against the plugin's
    /// currently-declared hook set: a mismatch means the decision predates
    /// a change to what the plugin asks for, so it must be treated as
    /// pending rather than honoured.
    pub(crate) fn get_plugin_approval_blocking(
        &self,
        plugin_id: &str,
    ) -> anyhow::Result<Option<PluginApprovalRow>> {
        let plugin_id = plugin_id.to_string();
        self.with_conn_blocking(move |conn| get_plugin_approval_query(conn, &plugin_id))
    }

    /// Persist an operator decision on a plugin's declared hook set.
    /// `hooks` is the canonical (sorted, newline-joined) hook list the
    /// decision was made against; `status` is [`APPROVAL_APPROVED`] or
    /// [`APPROVAL_DENIED`]. Upserts — re-deciding (or deciding after the
    /// plugin's hooks changed) overwrites the prior row.
    pub async fn set_plugin_approval(
        &self,
        plugin_id: &str,
        hooks: &str,
        status: &str,
    ) -> anyhow::Result<()> {
        let row = PluginApprovalRow {
            plugin_id: plugin_id.to_string(),
            hooks: hooks.to_string(),
            status: status.to_string(),
            decided_at: chrono::Utc::now().to_rfc3339(),
        };
        self.with_conn(move |conn| {
            diesel::insert_into(plugin_approvals::table)
                .values(&row)
                .on_conflict(plugin_approvals::plugin_id)
                .do_update()
                .set((
                    plugin_approvals::hooks.eq(&row.hooks),
                    plugin_approvals::status.eq(&row.status),
                    plugin_approvals::decided_at.eq(&row.decided_at),
                ))
                .execute(conn)?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn approval_round_trips_and_upserts() {
        let db = Db::in_memory().unwrap();

        // No decision yet.
        assert!(db.get_plugin_approval_blocking("api").unwrap().is_none());

        // Approve a hook set.
        db.set_plugin_approval("api", "http.request.before\ntodo", APPROVAL_APPROVED)
            .await
            .unwrap();
        let row = db.get_plugin_approval_blocking("api").unwrap().unwrap();
        assert_eq!(row.status, APPROVAL_APPROVED);
        assert_eq!(row.hooks, "http.request.before\ntodo");

        // Re-deciding upserts the single row (no duplicate), flipping the
        // status and recording the new hook set.
        db.set_plugin_approval("api", "http.request.before", APPROVAL_DENIED)
            .await
            .unwrap();
        let row = db.get_plugin_approval_blocking("api").unwrap().unwrap();
        assert_eq!(row.status, APPROVAL_DENIED);
        assert_eq!(row.hooks, "http.request.before");
    }
}
