-- Reverse of up.sql: move the plugin's stored rows back under the old
-- linux-app-manager id, with the same already-has-rows guard.
UPDATE plugin_data
SET plugin_id = 'linux-app-manager'
WHERE plugin_id = 'app-manager'
  AND NOT EXISTS (SELECT 1 FROM plugin_data WHERE plugin_id = 'linux-app-manager');

UPDATE plugin_session_meta
SET plugin_id = 'linux-app-manager'
WHERE plugin_id = 'app-manager'
  AND NOT EXISTS (SELECT 1 FROM plugin_session_meta WHERE plugin_id = 'linux-app-manager');
