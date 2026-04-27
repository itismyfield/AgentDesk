-- 0023_analytics_indices.sql
--
-- Issue #1243 — analytics endpoint performance.
--
-- The dashboard /api/streaks, /api/achievements, /api/activity-heatmap, and
-- /api/audit-logs queries scan task_dispatches / audit_logs by columns that
-- previously had no supporting index. On large test datasets the heatmap
-- (24 hourly bucket queries) and streak queries dominated p99 latency.
--
-- These indices are created with IF NOT EXISTS so the migration is idempotent
-- and safe to re-run on environments where someone has manually created an
-- equivalent index. We deliberately keep the predicate column order
-- (status / created_at) aligned with the most-selective filter the analytics
-- routes apply.

-- Streaks + achievements both filter on `status = 'completed'` and look up
-- by `to_agent_id`, then aggregate dates from `updated_at`. A composite
-- (to_agent_id, status) index gives the planner a fast covering lookup.
CREATE INDEX IF NOT EXISTS idx_task_dispatches_to_agent_status
    ON task_dispatches(to_agent_id, status);

-- Heatmap aggregates by hour-of-day for a given calendar day. Index on
-- created_at lets the date-range filter use an index scan instead of a
-- sequential one when the table grows beyond a few hundred MB.
CREATE INDEX IF NOT EXISTS idx_task_dispatches_created_at
    ON task_dispatches(created_at);

-- audit_logs is queried by (entity_type, entity_id) and ordered by timestamp
-- DESC. A composite index covers the common entity-scoped lookup, and a
-- separate timestamp DESC index covers the unfiltered "recent rows" path.
CREATE INDEX IF NOT EXISTS idx_audit_logs_entity_timestamp
    ON audit_logs(entity_type, entity_id, timestamp DESC);

CREATE INDEX IF NOT EXISTS idx_audit_logs_timestamp
    ON audit_logs(timestamp DESC);
