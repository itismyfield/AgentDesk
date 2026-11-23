//! KRX db test files that call root-crate items, declared here so they leave the KRX crate.
//! Each file stays in place; every `mod` keeps the `cfg(test)` chain of its original declaration.

#[path = "db/dispatched_sessions/canonical_identity_pg_tests.rs"]
pub(crate) mod canonical_identity_pg;
#[path = "db/auto_queue/run_owner_guard_pg_tests.rs"]
mod run_owner_guard_pg_tests;
#[path = "db/scheduled_messages/attachments_pg_tests.rs"]
mod scheduled_messages_attachments_pg;
