use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    /// Persist one held voice relay.
    pub async fn insert_voice_relay(&self, item: VoiceRelayItem) -> anyhow::Result<()> {
        self.with_conn(move |conn| {
            diesel::insert_into(voice_relay_queue::table)
                .values(&item)
                .execute(conn)?;
            Ok(())
        })
        .await
    }

    /// Every held voice relay, oldest first (boot-time reload).
    pub async fn list_voice_relays(&self) -> anyhow::Result<Vec<VoiceRelayItem>> {
        self.with_conn(move |conn| {
            voice_relay_queue::table
                .select(VoiceRelayItem::as_select())
                .order((
                    voice_relay_queue::created_at.asc(),
                    voice_relay_queue::id.asc(),
                ))
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Drop delivered (or obsolete) relays. Missing ids are ignored.
    pub async fn delete_voice_relays(&self, ids: Vec<String>) -> anyhow::Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        self.with_conn(move |conn| {
            diesel::delete(voice_relay_queue::table.filter(voice_relay_queue::id.eq_any(&ids)))
                .execute(conn)
                .map_err(Into::into)
        })
        .await
    }
}
