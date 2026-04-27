-- 0024_cancel_tombstones.sql
--
-- Persistent backing store for `cancel_induced_watcher_death` tombstones
-- (issue #1309). PR #1277 introduced an in-memory `RECENT_TURN_STOPS`
-- VecDeque keyed on (channel_id, tmux_session_name, stop_output_offset) so
-- the watcher cleanup path can suppress its 🔴 lifecycle notification when
-- the tmux death IS the cancel rather than a crash. That store is
-- process-local — a dcserver restart between the cancel and the watcher's
-- death observation drops the suppression evidence and the user sees both
-- the recovery handoff and the lifecycle notice.
--
-- This table is the durable mirror. The turn loop writes here on cancel
-- alongside the in-memory entry; the watcher reads (and DELETEs in the
-- same tx) on death detection when the in-memory store misses. Rows are
-- one-shot — consumed entries are gone — so the cap on the table is the
-- 10-minute TTL plus the periodic prune worker.

CREATE TABLE IF NOT EXISTS cancel_tombstones (
    id                 BIGSERIAL PRIMARY KEY,
    channel_id         BIGINT NOT NULL,
    tmux_session_name  TEXT,
    stop_output_offset BIGINT,
    reason             TEXT NOT NULL,
    recorded_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at         TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_cancel_tombstones_channel
    ON cancel_tombstones (channel_id, recorded_at DESC);

CREATE INDEX IF NOT EXISTS idx_cancel_tombstones_expires
    ON cancel_tombstones (expires_at);
