//! KRX db tests that call KE items (second db slice), declared here so they leave the KRX crate.
//! Each `#[path]` file stays in place; every `mod` keeps the `cfg(test)` chain of its original declaration.

mod auto_queue_dispatch_failure_tests;
mod auto_queue_entries_tests;
mod auto_queue_phase_gate_verdict_tests;
#[path = "../db/auto_queue/run_completion_pg_tests.rs"]
mod auto_queue_run_completion_pg;
#[path = "../db/replay_disposition_tests/compatibility_tests.rs"]
mod replay_disposition_compatibility;
#[path = "../db/replay_disposition_tests/consumer_tests.rs"]
mod replay_disposition_consumer;
#[path = "../db/session_observability/background_ordering_pg_tests.rs"]
mod session_observability_background_ordering_pg;
mod session_transcripts_clear_fence_tests;
#[path = "../db/status_check_migration_tests.rs"]
pub(crate) mod status_check_migration;
