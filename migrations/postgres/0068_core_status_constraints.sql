-- #2870: normalize and constrain stable core status columns.
--
-- Inventory used for this migration:
-- - agents.status: idle, working, archived (schema defaults + runtime resets + archive route)
-- - task_dispatches.status: pending, dispatched, completed, cancelled, failed
--   (already normalized/constrained by 0067_task_dispatches_status_check.sql)
-- - kanban_cards.status: default/simple/QA policy states
--   backlog, ready, requested, in_progress, review, qa_test, done, plus runtime
--   terminal/summary states referenced by server paths: cancelled, failed.
--
-- Kanban states can be extended by repo/agent pipeline_config, so this migration
-- enforces the stable slug shape at the DB layer instead of closing the enum and
-- breaking custom pipeline states.

UPDATE agents
SET status = CASE
    WHEN status IS NULL OR btrim(status) = '' THEN 'idle'
    WHEN lower(btrim(status)) IN ('idle', 'working', 'archived') THEN lower(btrim(status))
    WHEN lower(btrim(status)) IN ('active', 'busy', 'running') THEN 'working'
    WHEN lower(btrim(status)) IN ('disabled', 'deleted', 'inactive') THEN 'archived'
    ELSE 'idle'
END;

ALTER TABLE agents
    ALTER COLUMN status SET DEFAULT 'idle',
    ALTER COLUMN status SET NOT NULL,
    ADD CONSTRAINT agents_status_known_check
        CHECK (status IN ('idle', 'working', 'archived'));

UPDATE kanban_cards
SET status = CASE
    WHEN status IS NULL OR btrim(status) = '' THEN 'backlog'
    WHEN lower(btrim(status)) IN ('todo', 'to-do', 'to_do') THEN 'backlog'
    WHEN lower(btrim(status)) IN ('in progress', 'in-progress', 'doing', 'started') THEN 'in_progress'
    WHEN lower(btrim(status)) IN ('qa', 'qa test', 'qa-test') THEN 'qa_test'
    WHEN lower(btrim(status)) IN ('complete', 'completed', 'closed') THEN 'done'
    WHEN lower(btrim(status)) IN ('canceled') THEN 'cancelled'
    ELSE regexp_replace(lower(btrim(status)), '[^a-z0-9_]+', '_', 'g')
END;

UPDATE kanban_cards
SET status = btrim(status, '_');

UPDATE kanban_cards
SET status = 'backlog'
WHERE status = ''
   OR status !~ '^[a-z]';

ALTER TABLE kanban_cards
    ALTER COLUMN status SET DEFAULT 'backlog',
    ALTER COLUMN status SET NOT NULL,
    ADD CONSTRAINT kanban_cards_status_slug_check
        CHECK (status ~ '^[a-z][a-z0-9_]*$');
