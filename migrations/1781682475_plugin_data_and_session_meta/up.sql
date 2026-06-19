-- Generic, plugin-owned storage so a plugin can keep durable structured data
-- and tag sessions with its own metadata WITHOUT core gaining feature-specific
-- columns. Both are namespaced by plugin id. Used by the host functions in
-- src/plugin/host.rs (gated by the `data_store` / `session_write` permissions).

-- A plugin's document store: opaque JSON documents keyed by (plugin, collection,
-- key). `data` is genuinely free-form per-plugin state core never queries into,
-- so JSON-in-TEXT is appropriate here.
CREATE TABLE IF NOT EXISTS plugin_data (
    plugin_id   TEXT NOT NULL,
    collection  TEXT NOT NULL,
    key         TEXT NOT NULL,
    data        TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (plugin_id, collection, key)
);

-- Plugin-namespaced metadata attached to a core session, so "what is an expert"
-- (kind, knowledge summary, scope, etc.) lives in the plugin, not core columns.
-- One JSON blob per (session, plugin).
CREATE TABLE IF NOT EXISTS plugin_session_meta (
    session_id  TEXT NOT NULL,
    plugin_id   TEXT NOT NULL,
    data        TEXT NOT NULL,
    updated_at  TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (session_id, plugin_id)
);

CREATE INDEX IF NOT EXISTS idx_plugin_session_meta_plugin
    ON plugin_session_meta (plugin_id);
