-- #3865 DB retention: archive side table for `turns`.
--
-- The weekly `services::maintenance::jobs::db_retention` job applies a
-- 90-day archive-then-delete policy to `turns` (token/cost analytics), mirroring
-- the `session_transcripts` → `session_transcripts_archive` flow in 0016. Old
-- rows are copied here (idempotently, via WHERE NOT EXISTS) before being deleted
-- from the hot table, so historical token/cost totals stay queryable while the
-- INSERT-only `turns` table stays bounded.
--
-- No DDL is needed for `turn_lifecycle_events` or `skill_usage`: those follow a
-- plain time-window DELETE on their existing `created_at` / `used_at` columns
-- (already indexed by 0037 and 0062).

-- Archive table for `turns`. Mirrors the source columns and appends
-- `archived_at` to record when the retention sweep relocated each row.
CREATE TABLE IF NOT EXISTS turns_archive (
    turn_id             TEXT PRIMARY KEY,
    session_key         TEXT,
    thread_id           TEXT,
    thread_title        TEXT,
    channel_id          TEXT NOT NULL,
    agent_id            TEXT,
    provider            TEXT,
    session_id          TEXT,
    dispatch_id         TEXT,
    started_at          TIMESTAMPTZ NOT NULL,
    finished_at         TIMESTAMPTZ NOT NULL,
    duration_ms         INTEGER,
    input_tokens        INTEGER NOT NULL DEFAULT 0,
    cache_create_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens   INTEGER NOT NULL DEFAULT 0,
    output_tokens       INTEGER NOT NULL DEFAULT 0,
    created_at          TIMESTAMPTZ,
    archived_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_turns_archive_finished_at
    ON turns_archive(finished_at DESC);

CREATE INDEX IF NOT EXISTS idx_turns_archive_session_key
    ON turns_archive(session_key);
