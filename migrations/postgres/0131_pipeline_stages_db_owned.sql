-- pipeline_stages is edited through /api/pipeline/stages and the dashboard;
-- default-pipeline.yaml declares card states, not stages. 0019 marked the
-- table file-canonical, so every stage write returned 405, and startup copied
-- the YAML state ids into it as '__default__' rows that no code reads.
DROP TABLE IF EXISTS db_table_metadata;
DELETE FROM pipeline_stages WHERE repo_id = '__default__';

-- The runtime reads only id, stage_name, stage_order, trigger_after, provider,
-- agent_override_id and skip_condition (policies/pipeline.js,
-- policies/review-automation.js).
ALTER TABLE pipeline_stages
    DROP COLUMN IF EXISTS entry_skill,
    DROP COLUMN IF EXISTS timeout_minutes,
    DROP COLUMN IF EXISTS on_failure,
    DROP COLUMN IF EXISTS on_failure_target,
    DROP COLUMN IF EXISTS max_retries,
    DROP COLUMN IF EXISTS backoff,
    DROP COLUMN IF EXISTS parallel_with;

-- Nothing inserts into dispatch_queue; only a timeout sweep and a status count
-- still touched it.
DROP TABLE IF EXISTS dispatch_queue;
