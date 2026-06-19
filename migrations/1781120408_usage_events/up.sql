-- Per-turn token usage capture. One row per agent turn that reported
-- usage (the Claude provider surfaces it on the stream-json `result`
-- event, which Peckboard now mirrors here via a dedicated `agent-usage`
-- event). Project/card/expert attribution is intentionally NOT
-- denormalized — derive it downstream by joining `session_id` to the
-- `sessions` row, which already carries project_id/card_id/is_expert/
-- expert_kind. Operation attribution (Edit/Write/ask_expert) is derived
-- downstream from the existing `agent-tool-start` events in the same
-- turn window, so no separate tool-ops table is needed.
--
-- FKs: session_id cascades (usage history dies with the session, the
-- same way its events do); event_id is a nullable back-link to the
-- originating `events` row and is SET NULL if that event is purged
-- (e.g. /clear) so the usage row survives without a dangling pointer.

CREATE TABLE IF NOT EXISTS usage_events (
    id                    TEXT    PRIMARY KEY NOT NULL,
    session_id            TEXT    NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    event_id              TEXT    REFERENCES events(id) ON DELETE SET NULL,
    turn_seq              INTEGER,
    ts                    INTEGER NOT NULL,
    input_tokens          INTEGER NOT NULL DEFAULT 0,
    output_tokens         INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
    cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
    total_tokens          INTEGER NOT NULL DEFAULT 0,
    context_tokens        INTEGER NOT NULL DEFAULT 0,
    model                 TEXT
);

CREATE INDEX IF NOT EXISTS idx_usage_events_session
    ON usage_events (session_id, ts);

-- Backfill from any `agent-usage` events already in the log so historical
-- sessions aren't blank. On DBs created before this feature there are no
-- such events yet, so this is a no-op today; it also self-heals any row
-- whose live mirror-write failed. Keyed on the camelCase data shape that
-- `ProviderEvent::Usage` serializes (see src/provider/stream.rs). The
-- WHERE NOT EXISTS guard keeps it idempotent.
INSERT INTO usage_events (
    id, session_id, event_id, turn_seq, ts,
    input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
    total_tokens, context_tokens, model
)
SELECT
    lower(hex(randomblob(16))),
    e.session_id,
    e.id,
    ROW_NUMBER() OVER (PARTITION BY e.session_id ORDER BY e.ts, e.seq),
    e.ts,
    COALESCE(json_extract(e.data, '$.inputTokens'), 0),
    COALESCE(json_extract(e.data, '$.outputTokens'), 0),
    COALESCE(json_extract(e.data, '$.cacheReadTokens'), 0),
    COALESCE(json_extract(e.data, '$.cacheCreationTokens'), 0),
    COALESCE(json_extract(e.data, '$.totalTokens'), 0),
    COALESCE(json_extract(e.data, '$.contextTokens'), 0),
    json_extract(e.data, '$.model')
FROM events e
WHERE e.kind = 'agent-usage'
  AND NOT EXISTS (SELECT 1 FROM usage_events u WHERE u.event_id = e.id);
