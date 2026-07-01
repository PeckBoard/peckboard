-- Model-switch handover. When a session's model changes to one whose
-- (provider, account) continuity key differs, the running provider's
-- conversation can't be resumed by the new one, so the outgoing model
-- writes a handover document that the incoming model reads.
--
-- `handover_to_model` holds the target model id while the outgoing model
-- is generating the handover doc (i.e. a switch is mid-flight). It is
-- cleared once the switch finalizes.
--
-- `pending_handover_doc` holds the generated document from the moment the
-- switch finalizes until the incoming model consumes it on its first turn.
--
-- Both are nullable + additive, so existing rows need no backfill.
ALTER TABLE sessions ADD COLUMN handover_to_model TEXT;
ALTER TABLE sessions ADD COLUMN pending_handover_doc TEXT;
