-- Custom TTS pronunciations (`service::tts::lexicon`), checked before
-- misaki's dictionary. `word` is the lowercased match key, `display` the
-- word as typed. `source` is 'default' for built-in seeds, 'user' once
-- edited. Brand-new tables only — no existing table is touched.
CREATE TABLE IF NOT EXISTS tts_lexicon (
    word        TEXT PRIMARY KEY NOT NULL,
    display     TEXT NOT NULL,
    respelling  TEXT,
    phonemes    TEXT NOT NULL,
    source      TEXT NOT NULL CHECK (source IN ('default', 'user')),
    updated_at  TEXT NOT NULL
);

-- Every built-in default ever seeded. Seeding skips words listed here, so a
-- default the user deleted stays deleted, while defaults added by a later
-- release still get seeded once.
CREATE TABLE IF NOT EXISTS tts_lexicon_seeded (
    word       TEXT PRIMARY KEY NOT NULL,
    seeded_at  TEXT NOT NULL
);

-- Words misaki had no pronunciation for, with how often they were spoken.
CREATE TABLE IF NOT EXISTS tts_unknown_words (
    word        TEXT PRIMARY KEY NOT NULL,
    count       INTEGER NOT NULL,
    first_seen  TEXT NOT NULL,
    last_seen   TEXT NOT NULL
);
