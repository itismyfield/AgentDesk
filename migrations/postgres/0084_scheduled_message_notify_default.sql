-- Scheduled push messages are informational deliveries by default. The
-- announce bot is an authoritative agent-to-agent turn trigger, so using it as
-- the implicit default can wake the receiving agent and cause cascading work.
-- Keep explicit announce selections valid, but make omitted bot values safe.
ALTER TABLE scheduled_messages
    ALTER COLUMN bot SET DEFAULT 'notify';
