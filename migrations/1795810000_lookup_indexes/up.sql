-- Indexes found missing by the 2026-11 index audit. Every statement is
-- idempotent; `src/db/repair.rs::ensure_lookup_indexes` creates the same set
-- on startup for a DB that somehow skipped this migration.

-- `usage_events.event_id` is `REFERENCES events(id) ON DELETE SET NULL`:
-- without an index every deleted event row scanned all of `usage_events`
-- (session delete, clear, the retention sweeps). Also serves the startup
-- usage backfill's `NOT EXISTS (… u.event_id = e.id)`.
CREATE INDEX IF NOT EXISTS idx_usage_events_event ON usage_events (event_id);

-- Install-wide usage windows filter on `ts` alone; the existing indexes lead
-- with `session_id` / `account_id` and cannot serve them.
CREATE INDEX IF NOT EXISTS idx_usage_events_ts ON usage_events (ts);

-- `cards` had no index besides its primary key. The orchestrator lists a
-- project's cards in pickup order (`priority, created_at`) on every tick.
CREATE INDEX IF NOT EXISTS idx_cards_project ON cards (project_id, priority, created_at);

-- Both are foreign keys onto `sessions`: every session delete looked each of
-- them up with a full scan of `cards`.
CREATE INDEX IF NOT EXISTS idx_cards_worker_session ON cards (worker_session_id);
CREATE INDEX IF NOT EXISTS idx_cards_last_worker_session ON cards (last_worker_session_id);

-- Sessions of a card, newest first (one-running-worker-per-card check and the
-- card lifecycle counters, per card per orchestrator tick) and sessions of a
-- project (budget check per project per tick). Both are also foreign keys.
CREATE INDEX IF NOT EXISTS idx_sessions_card ON sessions (card_id, last_activity);
CREATE INDEX IF NOT EXISTS idx_sessions_project ON sessions (project_id);

-- Small tables whose session column is a foreign key onto `sessions` with no
-- usable index: each session delete scanned them.
CREATE INDEX IF NOT EXISTS idx_chat_focus_queue_source ON chat_focus_queue (source_session_id);
CREATE INDEX IF NOT EXISTS idx_voice_relay_queue_source ON voice_relay_queue (source_session_id);
CREATE INDEX IF NOT EXISTS idx_session_view_nodes_session ON session_view_nodes (session_id);
CREATE INDEX IF NOT EXISTS idx_dashboard_widgets_session ON dashboard_widgets (session_id);
CREATE INDEX IF NOT EXISTS idx_session_skills_session ON session_skills (session_id);

-- The two startup backfills in `repair.rs` read `events` by kind with no
-- session filter, which scanned the whole (large) table on every boot.
-- Partial indexes hold only those two kinds, so any other event insert pays
-- nothing for them. The usage one carries `id` so the backfill's
-- "already mirrored?" test never touches the table row.
CREATE INDEX IF NOT EXISTS idx_events_todo ON events (session_id, seq) WHERE kind = 'todo';
CREATE INDEX IF NOT EXISTS idx_events_agent_usage ON events (id) WHERE kind = 'agent-usage';

-- An exact duplicate of the index behind `UNIQUE (session_id, seq)` on
-- `events`: every event insert wrote both. Dropping it loses no data and no
-- query plan — the unique index serves the same lookups.
DROP INDEX IF EXISTS idx_events_session;
