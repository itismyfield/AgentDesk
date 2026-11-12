//! Entry status writes used by engine, dispatch, kanban and GitHub terminal paths.

pub(crate) use crate::db::auto_queue::{
    finalize_completed_dispatch_terminal_entry_on_pg_tx, sync_dispatch_terminal_entries_on_pg_tx,
    update_entry_status_on_pg, update_entry_status_on_pg_tx,
};
