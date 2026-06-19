-- Per-plugin user-configurable settings. Each row stores one
-- (plugin_id, key) pair with its value JSON-encoded; values may be
-- strings, numbers, booleans, or small objects (e.g. the key/value list
-- the Ollama plugin uses for additional headers). Built-in plugins
-- declare a settings_schema; this table holds the user-edited overrides.
CREATE TABLE IF NOT EXISTS plugin_settings (
    plugin_id   TEXT NOT NULL,
    key         TEXT NOT NULL,
    value       TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    PRIMARY KEY (plugin_id, key)
);

CREATE INDEX IF NOT EXISTS idx_plugin_settings_plugin
    ON plugin_settings (plugin_id);
