-- Plugin registry repositories: the set of static `registry.json` indexes
-- Peckboard aggregates installable plugins from. Each row is one registry
-- source. `url` is the resolved registry.json URL (a bare `owner/repo`
-- slug is resolved to its GitHub raw URL before storage); `label` is what
-- the operator entered (the slug or the URL).
CREATE TABLE IF NOT EXISTS plugin_repositories (
    url        TEXT NOT NULL PRIMARY KEY,
    label      TEXT NOT NULL,
    added_at   TEXT NOT NULL
);

-- Seed the canonical PeckBoard registry as a default. Removable — once an
-- operator deletes it, it stays deleted (this INSERT runs only on first
-- apply). `INSERT OR IGNORE` keeps a re-applied migration harmless.
INSERT OR IGNORE INTO plugin_repositories (url, label, added_at)
VALUES (
    'https://raw.githubusercontent.com/PeckBoard/plugins/main/registry.json',
    'PeckBoard/plugins',
    '2026-01-01T00:00:00Z'
);
