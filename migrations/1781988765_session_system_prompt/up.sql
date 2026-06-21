-- Per-session custom system prompt. When set, it fully replaces the
-- standing Peckboard system prompt for that session's agent runs (see
-- build_cli_args). Nullable + additive, so existing rows need no backfill.
ALTER TABLE sessions ADD COLUMN system_prompt TEXT;
