pub mod claim;
pub(crate) mod completion_write;
pub mod consultation;
pub mod entries;
pub mod phase_gate_verdict;
pub mod phase_gates;
pub mod queries;
pub(crate) mod run_admission;
pub(crate) mod run_owner_guard;
pub mod run_status;
pub mod runs;
pub mod slot_predicate;
pub mod slots;

#[cfg(test)]
mod run_admission_pg_tests;
#[cfg(test)]
mod run_completion_contract_tests;
#[cfg(test)]
mod run_completion_pg_tests;
#[cfg(test)]
mod run_owner_guard_pg_tests;
#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests;

pub use claim::*;
pub use consultation::*;
pub use entries::*;
pub use phase_gates::*;
pub use queries::*;
pub use runs::*;
pub use slots::*;
// Existing DB-path tests reach the service-owned completion writers under their old names.
#[cfg(test)]
pub(crate) use crate::services::auto_queue::transactions::{
    finalize_completed_dispatch_terminal_entry_on_pg_tx, finalize_run_if_ready_on_pg_tx,
    finalize_run_if_ready_pg, reconcile_phase_gate_for_terminal_dispatch_on_pg_tx,
    record_entry_dispatch_failure_on_pg, sync_dispatch_terminal_entries_on_pg_tx,
    update_entry_status_on_pg, update_entry_status_on_pg_tx,
};
