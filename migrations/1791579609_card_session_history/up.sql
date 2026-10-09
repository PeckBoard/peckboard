-- Card session history + review summary.
--
-- 1. Sealing: a worker session whose card moved on is no longer deleted by
--    the watchdog; it is sealed instead — the transcript stays readable but
--    no agent may ever run in it again (enforced at the dispatch chokepoint,
--    `SessionManager::send_message_locked`). NULL = live session.
ALTER TABLE sessions ADD COLUMN sealed_at TEXT NULL;
ALTER TABLE sessions ADD COLUMN sealed_reason TEXT NULL;

-- 2. The reviewer's verdict, kept apart from handoff_context (which the
--    next step overwrites).
ALTER TABLE cards ADD COLUMN review_summary TEXT NULL;
ALTER TABLE cards ADD COLUMN review_verdict TEXT NULL;
ALTER TABLE cards ADD COLUMN reviewed_at TEXT NULL;

-- 3. One row per worker claim on a card. `session_id` deliberately has no
--    FK: the row outlives a deleted session so the card keeps its history.
CREATE TABLE IF NOT EXISTS card_sessions (
    id          TEXT PRIMARY KEY NOT NULL,
    card_id     TEXT NOT NULL REFERENCES cards(id),
    session_id  TEXT NOT NULL,
    step        TEXT NOT NULL,
    role        TEXT NOT NULL,
    model       TEXT,
    started_at  TEXT NOT NULL,
    ended_at    TEXT,
    outcome     TEXT,
    summary     TEXT
);

CREATE INDEX IF NOT EXISTS idx_card_sessions_card
    ON card_sessions (card_id, started_at);
CREATE INDEX IF NOT EXISTS idx_card_sessions_session
    ON card_sessions (session_id);

-- Backfill: one row per surviving worker session still attached to a card.
-- Deterministic ids + INSERT OR IGNORE keep it idempotent.
INSERT OR IGNORE INTO card_sessions
    (id, card_id, session_id, step, role, model, started_at, ended_at, outcome, summary)
SELECT
    'backfill-' || s.id,
    s.card_id,
    s.id,
    COALESCE(
        s.worker_step,
        (SELECT json_extract(e.data, '$.from') FROM events e
          WHERE e.session_id = s.id AND e.kind = 'step-change'
            AND json_extract(e.data, '$.from') IS NOT NULL
          ORDER BY e.ts ASC, e.seq ASC LIMIT 1),
        CASE WHEN s.name LIKE 'review: %' THEN 'review' ELSE c.step END
    ),
    CASE WHEN s.name LIKE 'review: %' OR s.worker_step = 'review' THEN 'review' ELSE 'work' END,
    s.model,
    s.created_at,
    CASE WHEN c.worker_session_id = s.id THEN NULL ELSE s.last_activity END,
    CASE
        WHEN c.worker_session_id = s.id THEN NULL
        WHEN EXISTS (SELECT 1 FROM events e WHERE e.session_id = s.id AND e.kind = 'finish-requested')
            THEN CASE WHEN s.name LIKE 'review: %' OR s.worker_step = 'review'
                      THEN 'reviewed' ELSE 'finished' END
        WHEN EXISTS (SELECT 1 FROM events e WHERE e.session_id = s.id AND e.kind = 'complete-step-requested')
            THEN 'advanced'
        WHEN EXISTS (SELECT 1 FROM events e WHERE e.session_id = s.id AND e.kind = 'wont-do-requested')
            THEN 'wont_do'
        ELSE NULL
    END,
    (SELECT NULLIF(COALESCE(json_extract(e.data, '$.summary'),
                            json_extract(e.data, '$.handoffContext'),
                            json_extract(e.data, '$.reason')), '')
       FROM events e
      WHERE e.session_id = s.id
        AND e.kind IN ('finish-requested', 'complete-step-requested', 'wont-do-requested')
      ORDER BY e.ts DESC, e.seq DESC LIMIT 1)
FROM sessions s
JOIN cards c ON c.id = s.card_id
WHERE s.is_worker = 1;

-- Backfill the review summary of cards a reviewer finished: the reviewer's
-- finish_card summary is the card's handoff_context.
UPDATE cards
   SET review_summary = handoff_context,
       review_verdict = 'pass',
       reviewed_at = COALESCE(completed_at, updated_at)
 WHERE step = 'done'
   AND review_summary IS NULL
   AND handoff_context IS NOT NULL
   AND handoff_context <> ''
   AND (SELECT s.name FROM sessions s WHERE s.id = cards.last_worker_session_id) LIKE 'review: %';
