//! Entry status writes used by engine, dispatch, kanban, GitHub, auto-queue route and runtime
//! terminal paths, including the runtime per-entry dispatch-failure record.

pub(crate) use crate::db::auto_queue::{
    EntryDispatchFailureResult, finalize_completed_dispatch_terminal_entry_on_pg_tx,
    reconcile_failed_entry_done_on_pg, sync_dispatch_terminal_entries_on_pg_tx,
    update_entry_status_on_pg, update_entry_status_on_pg_tx,
};
