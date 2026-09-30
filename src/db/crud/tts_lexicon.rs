use diesel::prelude::*;
use diesel::upsert::excluded;

use crate::db::Db;
use crate::db::models::*;
use crate::db::schema::*;

impl Db {
    /// Every custom TTS pronunciation.
    pub async fn list_tts_lexicon(&self) -> anyhow::Result<Vec<TtsLexiconEntry>> {
        self.with_conn(move |conn| {
            tts_lexicon::table
                .select(TtsLexiconEntry::as_select())
                .order(tts_lexicon::display.asc())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Insert or replace a pronunciation and drop the word's unknown-word
    /// row: it has a pronunciation now.
    pub async fn upsert_tts_lexicon(&self, entry: TtsLexiconEntry) -> anyhow::Result<()> {
        self.with_conn(move |conn| {
            conn.transaction(|conn| {
                diesel::insert_into(tts_lexicon::table)
                    .values(&entry)
                    .on_conflict(tts_lexicon::word)
                    .do_update()
                    .set(&entry)
                    .execute(conn)?;
                diesel::delete(tts_unknown_words::table.find(&entry.word)).execute(conn)?;
                Ok(())
            })
        })
        .await
    }

    /// Delete a pronunciation; true if it existed.
    pub async fn delete_tts_lexicon(&self, word: String) -> anyhow::Result<bool> {
        self.with_conn(move |conn| {
            Ok(diesel::delete(tts_lexicon::table.find(&word)).execute(conn)? > 0)
        })
        .await
    }

    /// Seed built-in defaults never seeded before (tracked in
    /// `tts_lexicon_seeded`), without overwriting existing rows. A default
    /// the user deleted is therefore never re-added. Returns rows inserted.
    pub async fn seed_tts_lexicon(
        &self,
        defaults: Vec<TtsLexiconEntry>,
        now: String,
    ) -> anyhow::Result<usize> {
        self.with_conn(move |conn| {
            conn.transaction(|conn| {
                let seeded: Vec<String> = tts_lexicon_seeded::table
                    .select(tts_lexicon_seeded::word)
                    .load(conn)?;
                let mut inserted = 0;
                for entry in defaults.iter().filter(|e| !seeded.contains(&e.word)) {
                    inserted += diesel::insert_or_ignore_into(tts_lexicon::table)
                        .values(entry)
                        .execute(conn)?;
                    diesel::insert_or_ignore_into(tts_lexicon_seeded::table)
                        .values((
                            tts_lexicon_seeded::word.eq(&entry.word),
                            tts_lexicon_seeded::seeded_at.eq(&now),
                        ))
                        .execute(conn)?;
                }
                Ok(inserted)
            })
        })
        .await
    }

    /// Add `(word, count, first_seen, last_seen)` sightings to the
    /// unknown-word tally.
    pub async fn record_tts_unknown(
        &self,
        rows: Vec<(String, i64, String, String)>,
    ) -> anyhow::Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        self.with_conn(move |conn| {
            conn.transaction(|conn| {
                for (word, count, first, last) in &rows {
                    diesel::insert_into(tts_unknown_words::table)
                        .values((
                            tts_unknown_words::word.eq(word),
                            tts_unknown_words::count.eq(count),
                            tts_unknown_words::first_seen.eq(first),
                            tts_unknown_words::last_seen.eq(last),
                        ))
                        .on_conflict(tts_unknown_words::word)
                        .do_update()
                        .set((
                            tts_unknown_words::count
                                .eq(tts_unknown_words::count + excluded(tts_unknown_words::count)),
                            tts_unknown_words::last_seen.eq(excluded(tts_unknown_words::last_seen)),
                        ))
                        .execute(conn)?;
                }
                Ok(())
            })
        })
        .await
    }

    /// Unknown words, most spoken first.
    pub async fn list_tts_unknown(&self) -> anyhow::Result<Vec<TtsUnknownWord>> {
        self.with_conn(move |conn| {
            tts_unknown_words::table
                .select(TtsUnknownWord::as_select())
                .order((
                    tts_unknown_words::count.desc(),
                    tts_unknown_words::word.asc(),
                ))
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    /// Dismiss an unknown word; true if it existed.
    pub async fn delete_tts_unknown(&self, word: String) -> anyhow::Result<bool> {
        self.with_conn(move |conn| {
            Ok(diesel::delete(tts_unknown_words::table.find(&word)).execute(conn)? > 0)
        })
        .await
    }
}
