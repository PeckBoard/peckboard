-- Rename the linux-app-manager plugin to app-manager. The runtime plugin id
-- is the wasm file stem, and plugin_data / plugin_session_meta rows are keyed
-- by it, so without this every stored row (the user's configured remote SSH
-- targets above all) is orphaned under the old id. The plugin cannot migrate
-- this itself: a plugin can only ever read its own plugin id's rows.
--
-- Data-preserving UPDATEs only. Guarded to a no-op when the new id already
-- has rows, so a re-run (or a DB where the renamed plugin already wrote data)
-- never merges old rows into new ones.
UPDATE plugin_data
SET plugin_id = 'app-manager'
WHERE plugin_id = 'linux-app-manager'
  AND NOT EXISTS (SELECT 1 FROM plugin_data WHERE plugin_id = 'app-manager');

UPDATE plugin_session_meta
SET plugin_id = 'app-manager'
WHERE plugin_id = 'linux-app-manager'
  AND NOT EXISTS (SELECT 1 FROM plugin_session_meta WHERE plugin_id = 'app-manager');
