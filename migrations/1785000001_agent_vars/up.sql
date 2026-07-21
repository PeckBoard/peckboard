-- Agent variables: plain name/value state that agents READ AND WRITE via the
-- MCP tools (list_variables / set_variable / delete_variable) and users
-- manage in Settings. `folder_id` NULL = global; a folder-scoped var shadows
-- a global one with the same name for sessions in that folder. Unlike
-- env_vars these hold no secrets, are never encrypted, and are never
-- injected into command environments — agents read the values directly.
-- Partial indexes (not plain UNIQUE) because SQLite treats NULLs as distinct.
CREATE TABLE IF NOT EXISTS agent_vars (
    id         TEXT PRIMARY KEY NOT NULL,
    name       TEXT NOT NULL,
    value      TEXT NOT NULL,
    folder_id  TEXT,                 -- folders.id; NULL = global
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE UNIQUE INDEX idx_agent_vars_global_name ON agent_vars(name) WHERE folder_id IS NULL;
CREATE UNIQUE INDEX idx_agent_vars_folder_name ON agent_vars(folder_id, name) WHERE folder_id IS NOT NULL;
