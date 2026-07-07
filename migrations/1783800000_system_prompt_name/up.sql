-- Named library-prompt reference for sessions and cards. Sessions keep
-- their resolved `system_prompt` body as the runtime source of truth;
-- this column records WHICH library prompt (by name) was selected so the
-- UI/MCP can show and re-resolve it. Cards store only the name and resolve
-- to a body when a worker session is spawned. Nullable + additive, so
-- existing rows need no backfill.
ALTER TABLE sessions ADD COLUMN system_prompt_name TEXT;
ALTER TABLE cards ADD COLUMN system_prompt_name TEXT;
