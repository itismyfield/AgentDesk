-- Migration 0008: ALTER all INT4 columns expected as i64 by Rust to BIGINT.
--
-- adk-cdx PG migration created many integer columns as INT4 (default for INTEGER
-- keyword) but Rust callers consistently decode them as i64 via try_get::<i64,_>.
-- This produced runtime "mismatched types: i64 vs INT4" errors that blocked
-- dispatch_outbox delivery, dispatched_sessions hooks, agent metadata loads,
-- auto-queue activate intent, and other PG paths.
--
-- Rather than scatter ::BIGINT casts across 60+ SELECT sites, normalize the
-- columns themselves. PG accepts implicit upcast for INSERT bindings of i32,
-- so existing write paths continue to work without code changes.

ALTER TABLE agents ALTER COLUMN xp TYPE BIGINT;
ALTER TABLE agents ALTER COLUMN sprite_number TYPE BIGINT;

ALTER TABLE auto_queue_entries ALTER COLUMN priority_rank TYPE BIGINT;
ALTER TABLE auto_queue_entries ALTER COLUMN retry_count TYPE BIGINT;
ALTER TABLE auto_queue_entries ALTER COLUMN slot_index TYPE BIGINT;
ALTER TABLE auto_queue_entries ALTER COLUMN thread_group TYPE BIGINT;
ALTER TABLE auto_queue_entries ALTER COLUMN batch_phase TYPE BIGINT;

ALTER TABLE auto_queue_phase_gates ALTER COLUMN phase TYPE BIGINT;
ALTER TABLE auto_queue_phase_gates ALTER COLUMN next_phase TYPE BIGINT;

ALTER TABLE auto_queue_runs ALTER COLUMN timeout_minutes TYPE BIGINT;
ALTER TABLE auto_queue_runs ALTER COLUMN max_concurrent_threads TYPE BIGINT;
ALTER TABLE auto_queue_runs ALTER COLUMN thread_group_count TYPE BIGINT;

ALTER TABLE auto_queue_slots ALTER COLUMN slot_index TYPE BIGINT;
ALTER TABLE auto_queue_slots ALTER COLUMN assigned_thread_group TYPE BIGINT;

ALTER TABLE card_review_state ALTER COLUMN review_round TYPE BIGINT;
ALTER TABLE card_review_state ALTER COLUMN approach_change_round TYPE BIGINT;
ALTER TABLE card_review_state ALTER COLUMN session_reset_round TYPE BIGINT;

ALTER TABLE dispatch_outbox ALTER COLUMN retry_count TYPE BIGINT;

ALTER TABLE kanban_cards ALTER COLUMN github_issue_number TYPE BIGINT;
ALTER TABLE kanban_cards ALTER COLUMN review_round TYPE BIGINT;
ALTER TABLE kanban_cards ALTER COLUMN depth TYPE BIGINT;
ALTER TABLE kanban_cards ALTER COLUMN sort_order TYPE BIGINT;

ALTER TABLE pipeline_stages ALTER COLUMN stage_order TYPE BIGINT;
ALTER TABLE pipeline_stages ALTER COLUMN timeout_minutes TYPE BIGINT;
ALTER TABLE pipeline_stages ALTER COLUMN max_retries TYPE BIGINT;

ALTER TABLE pr_tracking ALTER COLUMN pr_number TYPE BIGINT;
ALTER TABLE pr_tracking ALTER COLUMN review_round TYPE BIGINT;
ALTER TABLE pr_tracking ALTER COLUMN retry_count TYPE BIGINT;

ALTER TABLE review_decisions ALTER COLUMN item_index TYPE BIGINT;

ALTER TABLE session_termination_events ALTER COLUMN last_offset TYPE BIGINT;
ALTER TABLE session_termination_events ALTER COLUMN tmux_alive TYPE BIGINT;

ALTER TABLE sessions ALTER COLUMN tokens TYPE BIGINT;

ALTER TABLE task_dispatches ALTER COLUMN chain_depth TYPE BIGINT;
ALTER TABLE task_dispatches ALTER COLUMN retry_count TYPE BIGINT;

ALTER TABLE turns ALTER COLUMN duration_ms TYPE BIGINT;
ALTER TABLE turns ALTER COLUMN input_tokens TYPE BIGINT;
ALTER TABLE turns ALTER COLUMN cache_create_tokens TYPE BIGINT;
ALTER TABLE turns ALTER COLUMN cache_read_tokens TYPE BIGINT;
ALTER TABLE turns ALTER COLUMN output_tokens TYPE BIGINT;
