-- Cost-aware model auto-switch ("Frugal Mode") opt-in.
--
-- When set, a session may switch itself to a cheaper same-provider model
-- after it has produced an implementation plan (see the model-control MCP
-- tools). The flag is a tri-state stored as nullable BOOLEAN:
--   NULL  = inherit the default (ON for worker sessions, OFF for chats),
--   TRUE  = force on, FALSE = force off.
-- Nullable + additive, so existing rows need no backfill (NULL = inherit).
--
-- The card column is the create/update surface; a spawned worker session
-- copies the card's resolved value onto its own row.
ALTER TABLE sessions ADD COLUMN model_autoswitch BOOLEAN;
ALTER TABLE cards ADD COLUMN model_autoswitch BOOLEAN;
