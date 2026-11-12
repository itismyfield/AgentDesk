//! Run finalization and completion notice for the engine, GitHub sync and auto-queue routes,
//! plus the run phase-gate repair whose terminal reconcile can finalize the run.

pub(crate) use crate::db::auto_queue::{
    finalize_run_if_ready_on_pg_tx, finalize_run_if_ready_pg, maybe_finalize_run_if_ready_pg,
    queue_run_completion_notify_on_pg, repair_phase_gates_for_run_on_pg,
};
