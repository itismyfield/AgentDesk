-- Meetings, agent quality rollups/alerts and XP are removed. agent_quality_event
-- stays: routines, scheduled delivery and /api/analytics still read turn events.
DROP TABLE IF EXISTS meeting_transcripts;
DROP TABLE IF EXISTS meetings;
DELETE FROM kv_meta
 WHERE key ~ '^meeting(:|_issue_repo:|_query_hash(_lookup)?:|_thread_hash(_lookup)?:)';

DROP TABLE IF EXISTS quality_regression_cooldowns;
DROP TABLE IF EXISTS agent_prompt_commits;
DROP TABLE IF EXISTS agent_quality_daily;

ALTER TABLE agents DROP COLUMN IF EXISTS xp;
