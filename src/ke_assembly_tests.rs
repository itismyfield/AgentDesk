//! ROOT tests that assemble KE operations with the Discord registry and fixtures.
#[path = "services/turn_lifecycle/policy_observability_root_tests.rs"]
pub(crate) mod policy_observability_root_tests;
#[path = "services/turn_orchestrator/queued_head_root_tests.rs"]
mod queued_head_root_tests;
#[path = "services/auto_queue/runtime/slot_reset_host_pg_tests.rs"]
pub(crate) mod slot_reset_host_pg_tests;
#[path = "engine/ops/turn_ops_root_tests.rs"]
mod turn_ops_root_tests;
#[cfg(unix)]
#[path = "services/auto_queue/runtime/verified_reset_pg_tests.rs"]
mod verified_reset_pg_tests;
