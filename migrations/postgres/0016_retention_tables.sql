-- #1093 (909-4) DB retention: archive + monthly aggregate side tables.
--
-- Five retention policies managed by `services::maintenance::jobs::db_retention`:
--   1. agent_quality_event (turn_*)  → monthly aggregate then DELETE >90d raw
--   2. session_transcripts           → COPY to archive then DELETE >90d
--   3. message_outbox (sent)         → DELETE >7d
--   4. auto_queue_entries (completed)→ DELETE >30d
--   5. task_dispatches   (completed) → monthly aggregate then DELETE >90d
--
-- `kanban_cards` is explicitly excluded (permanent retention).

-- 1) Archive table for session_transcripts.
--    LIKE ... INCLUDING ALL preserves columns + defaults + indexes (but drops
--    the generated `search_tsv` dependency; we accept that since archive search
--    is not a target use case).
CREATE TABLE IF NOT EXISTS session_transcripts_archive (
    id                BIGINT PRIMARY KEY,
    turn_id           TEXT NOT NULL,
    session_key       TEXT,
    channel_id        TEXT,
    agent_id          TEXT,
    provider          TEXT,
    dispatch_id       TEXT,
    user_message      TEXT NOT NULL DEFAULT '',
    assistant_message TEXT NOT NULL DEFAULT '',
    events_json       JSONB NOT NULL DEFAULT '[]'::jsonb,
    duration_ms       INTEGER,
    created_at        TIMESTAMPTZ,
    archived_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_session_transcripts_archive_created_at
    ON session_transcripts_archive(created_at DESC);

CREATE INDEX IF NOT EXISTS idx_session_transcripts_archive_session_key
    ON session_transcripts_archive(session_key);

-- 2) Monthly aggregate for turn analytics (agent_quality_event turn_*).
CREATE TABLE IF NOT EXISTS turn_analytics_monthly_aggregate (
    month          DATE PRIMARY KEY,
    total_turns    BIGINT NOT NULL DEFAULT 0,
    success_count  BIGINT NOT NULL DEFAULT 0,
    error_count    BIGINT NOT NULL DEFAULT 0,
    start_count    BIGINT NOT NULL DEFAULT 0,
    aggregated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- 3) Monthly aggregate for task_dispatches completed rows.
CREATE TABLE IF NOT EXISTS task_dispatches_monthly_aggregate (
    month             DATE PRIMARY KEY,
    total_dispatches  BIGINT NOT NULL DEFAULT 0,
    completed_count   BIGINT NOT NULL DEFAULT 0,
    review_count      BIGINT NOT NULL DEFAULT 0,
    aggregated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
