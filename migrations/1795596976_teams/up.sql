-- Teams of PeckBoard servers (team.peckboard.com). New tables only — no
-- existing table is touched.

-- A team this server founded, belongs to, has asked to join, or was in.
-- `state`: 'member'; 'pending' (join request filed, not yet approved);
-- 'left' / 'removed' (kept so the user sees what happened; forgettable).
-- `log_len` + `log_head` pin the newest membership-log state this server
-- has verified: a later fetch must extend it, so the team service cannot
-- hide a removal by serving older history.
-- `request_id`, `invite_id`, `invite_secret` are set only while a join
-- request is pending (they produce the verification code) and cleared when
-- it settles.
CREATE TABLE IF NOT EXISTS teams (
    id             TEXT PRIMARY KEY NOT NULL,
    name           TEXT NOT NULL,
    state          TEXT NOT NULL CHECK (state IN ('member', 'pending', 'left', 'removed')),
    role           TEXT CHECK (role IS NULL OR role IN ('owner', 'admin', 'member')),
    log_len        BIGINT NOT NULL DEFAULT 0,
    log_head       TEXT NOT NULL DEFAULT '',
    request_id     TEXT,
    invite_id      TEXT,
    invite_secret  TEXT,
    last_error     TEXT,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL
);

-- This server's verified copy of a team's membership log, one signed
-- record per row. `record` is the record exactly as the team service
-- serves it (JSON): an opaque signed blob that is only ever replayed
-- whole, never queried into.
CREATE TABLE IF NOT EXISTS team_log (
    team_id  TEXT NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    seq      BIGINT NOT NULL,
    record   TEXT NOT NULL,
    PRIMARY KEY (team_id, seq)
);

-- Invites this server created and that are still open. The secret is kept
-- here (never sent to the team service) because this server needs it to
-- check a join request and show its verification code. Rows are deleted
-- when the invite is used, withdrawn or expired.
CREATE TABLE IF NOT EXISTS team_invites (
    invite_id   TEXT PRIMARY KEY NOT NULL,
    team_id     TEXT NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    secret      TEXT NOT NULL,
    expires_at  TEXT NOT NULL,
    created_at  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_team_invites_team ON team_invites (team_id);
