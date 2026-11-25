DROP INDEX IF EXISTS idx_folder_memories_folder;
DROP TABLE IF EXISTS folder_memories;
ALTER TABLE session_memories DROP COLUMN expires_at;
