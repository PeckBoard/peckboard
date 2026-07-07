-- Named, reusable system prompts that steer a session's model toward a
-- kind of work (implement / research / debug / review / docs / ...). The
-- cost-aware auto-switch applies a matching prompt when it downgrades a
-- session, and the Settings > System Prompts page manages the library.
--
-- `name` is the stable handle callers reference (UNIQUE so a re-import by
-- name updates in place). `source_url` records where an imported prompt
-- came from so it can be refreshed later (NULL for hand-written prompts).
-- Nothing else in the schema references this table, so it is a clean,
-- self-contained addition.
CREATE TABLE IF NOT EXISTS system_prompts (
    id          TEXT PRIMARY KEY NOT NULL,
    name        TEXT NOT NULL UNIQUE,
    body        TEXT NOT NULL,
    source_url  TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
