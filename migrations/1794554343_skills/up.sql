-- Skills: reusable, versioned agent instructions (SKILL.md = frontmatter +
-- markdown body) that can be attached to sessions directly or through
-- groups. New tables only — no existing table is touched.
--
-- Scopes: `local` (DB only), `team` (a private git repo source cloned under
-- <data_dir>/skills/<source_id>), `public` (an index.json URL source).

-- Where non-local skills come from. `kind` is `public` (index URL) or
-- `team` (git URL). `secret_ref` names the credential used to reach a team
-- repo (`ssh_key:<id>` or `env:<NAME>`) — never the secret itself.
CREATE TABLE IF NOT EXISTS skill_sources (
    id            TEXT PRIMARY KEY NOT NULL,
    kind          TEXT NOT NULL CHECK (kind IN ('public', 'team')),
    name          TEXT NOT NULL UNIQUE,
    url           TEXT NOT NULL,
    branch        TEXT NOT NULL DEFAULT 'main',
    secret_ref    TEXT,
    last_sync_at  TEXT,
    last_error    TEXT,
    created_at    TEXT NOT NULL
);

-- Installed skills: every local skill plus every subscribed remote one.
-- `version` is the newest stored version; `tags` is a JSON string array
-- (display/search metadata only). Group membership lives in
-- skill_group_members.
CREATE TABLE IF NOT EXISTS skills (
    id           TEXT PRIMARY KEY NOT NULL,
    scope        TEXT NOT NULL CHECK (scope IN ('local', 'team', 'public')),
    source_id    TEXT REFERENCES skill_sources(id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    description  TEXT NOT NULL DEFAULT '',
    tags         TEXT NOT NULL DEFAULT '[]',
    version      TEXT NOT NULL,
    created_by   TEXT,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);

-- One name per source (local skills share the '' source key).
CREATE UNIQUE INDEX IF NOT EXISTS idx_skills_source_name
    ON skills (COALESCE(source_id, ''), name);

-- Immutable version history. A version row is never updated: an edit
-- writes a new (bumped) version.
CREATE TABLE IF NOT EXISTS skill_versions (
    id          TEXT PRIMARY KEY NOT NULL,
    skill_id    TEXT NOT NULL REFERENCES skills(id) ON DELETE CASCADE,
    version     TEXT NOT NULL,
    body        TEXT NOT NULL,
    sha256      TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    UNIQUE (skill_id, version)
);

CREATE INDEX IF NOT EXISTS idx_skill_versions_skill
    ON skill_versions (skill_id);

-- A remote skill the user subscribed to. `pinned_version` NULL = follow
-- the latest version on every sync.
CREATE TABLE IF NOT EXISTS skill_subscriptions (
    skill_id        TEXT PRIMARY KEY NOT NULL REFERENCES skills(id) ON DELETE CASCADE,
    pinned_version  TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

-- Named bundles of skills, by skill name (a group may name skills that are
-- not installed yet).
CREATE TABLE IF NOT EXISTS skill_groups (
    id           TEXT PRIMARY KEY NOT NULL,
    slug         TEXT NOT NULL,
    name         TEXT NOT NULL,
    description  TEXT NOT NULL DEFAULT '',
    scope        TEXT NOT NULL CHECK (scope IN ('local', 'team', 'public')),
    source_id    TEXT REFERENCES skill_sources(id) ON DELETE CASCADE,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_skill_groups_source_slug
    ON skill_groups (COALESCE(source_id, ''), slug);

CREATE TABLE IF NOT EXISTS skill_group_members (
    group_id    TEXT NOT NULL REFERENCES skill_groups(id) ON DELETE CASCADE,
    skill_name  TEXT NOT NULL,
    PRIMARY KEY (group_id, skill_name)
);

-- Skills assigned to a session: exactly one of skill_id / group_id.
CREATE TABLE IF NOT EXISTS session_skills (
    id          TEXT PRIMARY KEY NOT NULL,
    session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    skill_id    TEXT REFERENCES skills(id) ON DELETE CASCADE,
    group_id    TEXT REFERENCES skill_groups(id) ON DELETE CASCADE,
    created_at  TEXT NOT NULL,
    CHECK ((skill_id IS NULL) <> (group_id IS NULL))
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_session_skills_skill
    ON session_skills (session_id, skill_id) WHERE skill_id IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_session_skills_group
    ON session_skills (session_id, group_id) WHERE group_id IS NOT NULL;

-- Seed the canonical public catalog. Removable — once deleted it stays
-- deleted (this INSERT runs only on first apply). The runtime env var
-- PECKBOARD_SKILLS_INDEX_URL overrides its url without touching the row.
INSERT OR IGNORE INTO skill_sources (id, kind, name, url, branch, created_at)
VALUES (
    'public',
    'public',
    'Peckboard Public',
    'https://raw.githubusercontent.com/PeckBoard/skills/main/index.json',
    'main',
    '2026-11-13T00:00:00Z'
);
