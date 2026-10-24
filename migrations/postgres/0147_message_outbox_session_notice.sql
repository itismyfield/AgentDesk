-- Display metadata is opt-in at enqueue; legacy rows and workers keep their original surface.
ALTER TABLE message_outbox
    ADD COLUMN IF NOT EXISTS session_notice JSONB;
