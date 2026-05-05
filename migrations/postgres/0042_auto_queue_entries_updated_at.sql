ALTER TABLE auto_queue_entries
    ADD COLUMN IF NOT EXISTS updated_at TIMESTAMPTZ DEFAULT NOW();

CREATE INDEX IF NOT EXISTS idx_auto_queue_entries_run_status_updated_at
    ON auto_queue_entries(run_id, status, updated_at);
