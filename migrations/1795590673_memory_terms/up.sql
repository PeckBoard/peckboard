-- Long- and short-term memory.
--
-- Short-term: a session note with `expires_at` set is deleted once that time
-- passes (retention sweeper; reads skip it sooner). NULL = never expires —
-- every row that existed before this migration stays that way, so nobody's
-- notes disappear after an upgrade. The clock starts at `updated_at`.
ALTER TABLE session_memories ADD COLUMN expires_at TEXT;

-- Long-term: what a folder's sessions should always know — architecture,
-- design decisions, code structure, project layout, working conventions.
-- Shared by every session in the folder; never expires; goes away with the
-- folder (FK cascade). `source_session_id` records which session wrote the
-- note (informational, no FK: the note outlives that session).
CREATE TABLE IF NOT EXISTS folder_memories (
    id                 TEXT PRIMARY KEY NOT NULL,
    folder_id          TEXT NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    category           TEXT NOT NULL CHECK (category IN
                           ('architecture', 'decision', 'structure', 'layout', 'convention')),
    title              TEXT NOT NULL,
    content            TEXT NOT NULL,
    source_session_id  TEXT,
    created_at         TEXT NOT NULL,
    updated_at         TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_folder_memories_folder
    ON folder_memories (folder_id, category, created_at);
