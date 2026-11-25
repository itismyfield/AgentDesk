//! KRX services tests that call discord-layer items, declared here so they leave the KRX crate.
//! Each `#[path]` file stays in place and keeps the `cfg(test)` chain of its original declaration.

#[path = "../../cluster/intake_router_hook/owner_record/output_tests.rs"]
mod cluster_owner_record_output;
#[path = "../../herdr_launch_tests.rs"]
pub(crate) mod herdr_launch;
pub(crate) mod intake_worker_executor_tests;
mod krx_dedupe_dl_tests;
#[path = "../../message_outbox/plain_tests.rs"]
mod message_outbox_plain;
mod message_outbox_source_contract_tests;
