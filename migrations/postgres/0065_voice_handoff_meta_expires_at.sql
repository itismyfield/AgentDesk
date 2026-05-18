-- Add `expires_at` to voice_background_handoff_meta so the GC window can
-- be refreshed when the background turn's watchdog deadline is extended
-- (issue #2352).
--
-- Prior to this migration the GC sweep and live-row guards used
-- `created_at` alone, giving every row a fixed 24-hour window from
-- insertion.  When an operator calls the extend-timeout API the watchdog
-- deadline moves forward, but the handoff marker's effective lifetime did
-- not.  For turns that run beyond 24 hours the GC would delete the row
-- before terminal delivery claims it, silently dropping the spoken-summary
-- route.
--
-- `expires_at` carries the live deadline; `refresh_handoff_ttl_durable`
-- resets it to `NOW() + 24 h` on each watchdog extension.  The GC and all
-- live-row guards switch to `expires_at > NOW()` / `expires_at < NOW()`.

ALTER TABLE voice_background_handoff_meta
    ADD COLUMN IF NOT EXISTS expires_at TIMESTAMPTZ;

-- Back-fill existing rows: their original 24-hour window starts from
-- `created_at` so they keep exactly the remaining lifetime they had.
UPDATE voice_background_handoff_meta
    SET expires_at = created_at + INTERVAL '86400 seconds'
    WHERE expires_at IS NULL;

ALTER TABLE voice_background_handoff_meta
    ALTER COLUMN expires_at SET NOT NULL;

ALTER TABLE voice_background_handoff_meta
    ALTER COLUMN expires_at SET DEFAULT NOW() + INTERVAL '86400 seconds';

CREATE INDEX IF NOT EXISTS idx_voice_background_handoff_meta_expires_at
    ON voice_background_handoff_meta (expires_at);
