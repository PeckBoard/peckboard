-- OAuth 2.1 client_credentials clients for Peckboard's own MCP server
-- (`POST /mcp`). Each client is bound to one folder (and optionally one
-- project) and to a backing session whose row drives the tool-call scope.
-- Secrets and access tokens are stored as SHA-256 hashes only.
--
-- `session_id` deliberately has no FK: revoking a client leaves its session
-- row (and transcript) in place, and deleting the session must not be
-- blocked by the client row. `project_id` has no FK for the same reason —
-- a deleted project leaves the client scoped to a dead id, which every
-- scope check refuses.
CREATE TABLE IF NOT EXISTS oauth_clients (
    id            TEXT PRIMARY KEY NOT NULL,
    name          TEXT NOT NULL,
    client_id     TEXT NOT NULL UNIQUE,
    secret_hash   TEXT NOT NULL,
    folder_id     TEXT NOT NULL REFERENCES folders(id),
    project_id    TEXT,
    session_id    TEXT NOT NULL,
    created_by    TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    last_used_at  TEXT,
    revoked_at    TEXT
);

CREATE TABLE IF NOT EXISTS oauth_access_tokens (
    token_hash       TEXT PRIMARY KEY NOT NULL,
    oauth_client_id  TEXT NOT NULL REFERENCES oauth_clients(id),
    expires_at       TEXT NOT NULL,
    created_at       TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_oauth_access_tokens_client
    ON oauth_access_tokens (oauth_client_id);
CREATE INDEX IF NOT EXISTS idx_oauth_clients_folder
    ON oauth_clients (folder_id);
