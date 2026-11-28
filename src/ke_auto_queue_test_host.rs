//! KE auto_queue·db 시험 가운데 ROOT 항목을 쓰는 것을 ROOT binary에서 돌리는 host. `#[path]` 선언만 둔다.

#[path = "services/auto_queue/advisory_return_pg_tests.rs"]
mod advisory_return_pg_tests;
#[path = "db/auto_queue/run_completion_pg_tests.rs"]
mod auto_queue_run_completion_pg;
#[path = "db/auto_queue/tests.rs"]
mod auto_queue_store_tests;
#[path = "services/auto_queue/campaign_handoff_tests.rs"]
mod campaign_handoff_tests;
#[path = "services/auto_queue/cleanup_tasks_ownership_pg_tests.rs"]
mod cleanup_tasks_ownership_pg_tests;
#[path = "supervisor/decision_log_pg_tests.rs"]
mod decision_log_pg_tests;
#[path = "services/auto_queue/dispatched_entry_recovery_pg_tests.rs"]
mod dispatched_entry_recovery_pg_tests;
#[path = "db/dispatched_sessions/tests.rs"]
mod dispatched_sessions_store_tests;
#[path = "dispatch/fresh_worktree_pg_tests.rs"]
mod fresh_worktree_pg_tests;
#[path = "services/auto_queue/cancel_run/malformed_context_pg_tests.rs"]
mod malformed_context_pg_tests;
#[path = "db/server_campaigns/stage_store_tests.rs"]
mod server_campaigns_stage_store;
#[path = "db/server_campaigns/verification_store_tests.rs"]
mod server_campaigns_verification_store;
#[path = "services/krx_db_b_tests/session_transcripts_clear_fence_tests.rs"]
mod session_transcripts_clear_fence_tests;
