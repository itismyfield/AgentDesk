//! Card writes that move auto-queue entries in the same tx as the run completion notice.
//! The kanban delete, rereview and reopen routes reach the DB bodies only through these names.

pub(crate) use crate::db::kanban_cards::{
    move_auto_queue_entry_to_dispatched_on_pg, reactivate_done_auto_queue_entries_pg,
    soft_delete_card_pg,
};
