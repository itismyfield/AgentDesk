-- Store the Discord message used as the editable per-run routine log summary.
ALTER TABLE IF EXISTS routine_runs
    ADD COLUMN IF NOT EXISTS discord_message_id TEXT;

ALTER TABLE IF EXISTS routine_runs
    ADD COLUMN IF NOT EXISTS discord_log_sections JSONB NOT NULL DEFAULT '{}'::jsonb;
