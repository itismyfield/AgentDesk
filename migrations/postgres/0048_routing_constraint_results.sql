ALTER TABLE task_dispatches
    ADD COLUMN IF NOT EXISTS constraint_results JSONB;

ALTER TABLE dispatch_outbox
    ADD COLUMN IF NOT EXISTS constraint_results JSONB;
