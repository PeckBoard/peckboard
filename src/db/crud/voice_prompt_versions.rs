use diesel::dsl::sql;
use diesel::prelude::*;
use diesel::sql_types::BigInt;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    /// Append a voice-prompt version (it becomes the active one).
    pub async fn insert_voice_prompt_version(&self, row: VoicePromptVersion) -> anyhow::Result<()> {
        self.with_conn(move |conn| {
            diesel::insert_into(voice_prompt_versions::table)
                .values(&row)
                .execute(conn)?;
            Ok(())
        })
        .await
    }

    /// Up to `limit` voice-prompt versions, newest first. `rowid` breaks
    /// `created_at` ties (two saves within one clock tick).
    pub async fn list_voice_prompt_versions(
        &self,
        limit: i64,
    ) -> anyhow::Result<Vec<VoicePromptVersion>> {
        self.with_conn(move |conn| {
            voice_prompt_versions::table
                .select(VoicePromptVersion::as_select())
                .order((
                    voice_prompt_versions::created_at.desc(),
                    sql::<BigInt>("rowid").desc(),
                ))
                .limit(limit)
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }
}
