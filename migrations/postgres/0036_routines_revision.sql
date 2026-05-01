-- Routines revision: Discord thread lifecycle and per-routine agent timeout.

ALTER TABLE IF EXISTS routines
    ADD COLUMN IF NOT EXISTS discord_thread_id TEXT;

ALTER TABLE IF EXISTS routines
    ADD COLUMN IF NOT EXISTS timeout_secs INTEGER;

CREATE INDEX IF NOT EXISTS idx_routines_discord_thread_id
    ON routines(discord_thread_id)
    WHERE discord_thread_id IS NOT NULL;
