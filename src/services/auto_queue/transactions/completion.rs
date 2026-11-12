//! Run finalization used by the engine `completeRun` op and the GitHub terminal sync.

pub(crate) use crate::db::auto_queue::{finalize_run_if_ready_pg, maybe_finalize_run_if_ready_pg};
