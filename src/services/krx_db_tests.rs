//! KRX db test files that call KE items, declared here so they leave the KRX crate.
//! Each file stays in place; every `mod` keeps the `cfg(test)` chain of its original declaration.

#[path = "../db/auto_queue/tests.rs"]
mod auto_queue;
#[path = "../db/auto_queue/phase_gates/reconcile_phase_gate_pg_tests.rs"]
mod auto_queue_reconcile_phase_gate_pg;
#[path = "../db/calendar_sync/postgres_tests.rs"]
mod calendar_sync_postgres;
#[path = "../db/dispatched_sessions/tests.rs"]
mod dispatched_sessions;
mod dispatched_sessions_authority_tests;
#[path = "../db/dispatched_sessions/hosted_execution/legacy_return_tests.rs"]
mod dispatched_sessions_legacy_return;
mod dispatches_outbox_delivery_tests;
#[path = "../db/scheduled_messages/image_lifecycle_tests.rs"]
mod scheduled_messages_image_lifecycle;
#[path = "../db/server_campaigns/decision_gate_tests.rs"]
mod server_campaigns_decision_gate;
#[path = "../db/server_campaigns/evidence_store_tests.rs"]
mod server_campaigns_evidence_store;
#[path = "../db/server_campaigns/recovery_tests.rs"]
mod server_campaigns_recovery;
#[path = "../db/server_campaigns/stage_store_tests.rs"]
mod server_campaigns_stage_store;
#[path = "../db/server_campaigns/storage_tests.rs"]
pub(crate) mod server_campaigns_storage;
#[path = "../db/server_campaigns/verification_store_tests.rs"]
mod server_campaigns_verification_store;
mod session_transcripts_clear_fence_tests;
