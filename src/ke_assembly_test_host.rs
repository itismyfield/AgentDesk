//! ROOT tests that assemble KE operations with the Discord registry and fixtures.
#[path = "services/auto_queue/runtime/slot_reset_host_pg_tests.rs"]
pub(crate) mod slot_reset_host_pg_tests;
#[cfg(unix)]
#[path = "services/auto_queue/runtime/verified_reset_pg_tests.rs"]
mod verified_reset_pg_tests;
