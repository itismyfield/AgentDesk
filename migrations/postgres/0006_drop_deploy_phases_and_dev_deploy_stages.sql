-- Dev-runtime closure (#834 follow-up): retire deploy-pipeline residue.
--
-- 1) The `deploy_phases` column on `auto_queue_runs` was written by the
--    removed dev-deploy pipeline but no live code reads it.
-- 2) The `dev-deploy` and `e2e-test` pipeline stages for itismyfield/AgentDesk
--    were seeded for the deleted deploy-pipeline policy.

ALTER TABLE IF EXISTS auto_queue_runs
    DROP COLUMN IF EXISTS deploy_phases;

DELETE FROM pipeline_stages
 WHERE repo_id = 'itismyfield/AgentDesk'
   AND stage_name IN ('dev-deploy', 'e2e-test');
