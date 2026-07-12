-- Preserve the recurrence anchor of manual trigger-now attempts across
-- interrupted retries, and distinguish a reserved agent turn identity from a
-- headless turn that the runtime has actually confirmed as started.
ALTER TABLE scheduled_message_deliveries
    ADD COLUMN IF NOT EXISTS resume_scheduled_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS turn_started_at TIMESTAMPTZ;
