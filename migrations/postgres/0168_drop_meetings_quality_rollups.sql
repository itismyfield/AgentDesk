-- Meetings, agent quality rollups/alerts and XP are removed. agent_quality_event
-- stays: routines, scheduled delivery and /api/analytics still read turn events.
-- agents.xp drops in a later migration: during a peers-first deploy the old leader
-- policy still writes it before moving a card to review.
DROP TABLE IF EXISTS meeting_transcripts;
DROP TABLE IF EXISTS meetings;
DELETE FROM kv_meta
 WHERE key ~ '^meeting(:|_issue_repo:|_query_hash(_lookup)?:|_thread_hash(_lookup)?:)';

DROP TABLE IF EXISTS quality_regression_cooldowns;
DROP TABLE IF EXISTS agent_prompt_commits;
DROP TABLE IF EXISTS agent_quality_daily;
