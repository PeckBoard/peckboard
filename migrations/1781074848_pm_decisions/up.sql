-- PM-decision log: durable record of questions raised while managing a
-- project and the answers they received. A row is born 'pending'
-- (answer NULL) or directly 'answered'; once answered it is immutable
-- except via supersession, which inserts a replacement row and points
-- the old row's superseded_by at it (status 'superseded'). That makes
-- the table an append-only audit trail of what was decided and when.
--
-- asked_by_session_id is provenance only (NULL = user/PM-initiated)
-- and deliberately carries no FK: sessions are routinely deleted and
-- the decision log must outlive its asker.
--
-- project_id cascades like project_workflow_instructions: a decision
-- log is meaningless without its project, and a plain FK would block
-- delete_project_cascade.
CREATE TABLE IF NOT EXISTS pm_decisions (
    id                   TEXT PRIMARY KEY NOT NULL,
    project_id           TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    question             TEXT NOT NULL,
    answer               TEXT,
    status               TEXT NOT NULL DEFAULT 'pending',
    asked_by_session_id  TEXT,
    superseded_by        TEXT REFERENCES pm_decisions(id),
    created_at           TEXT NOT NULL,
    answered_at          TEXT
);

CREATE INDEX IF NOT EXISTS idx_pm_decisions_project_status
    ON pm_decisions (project_id, status);
