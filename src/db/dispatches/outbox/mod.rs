//! Dispatch outbox repository facade.
//!
//! Keep caller imports stable while responsibility-specific SQL lives in the
//! sibling modules below:
//! - `claim` owns claim-candidate selection and claim marking SQL.
//! - `delivery` owns dispatched/done/failed delivery state transitions.
//! - `retry` owns retry rescheduling.
//! - `followup` owns follow-up metadata reads and thread cleanup helpers.
//! - `notify` owns notify-row requeue/rearm behavior.
//! - `diagnostics` owns routing diagnostics persistence.
//! - `model` owns shared row/data shapes.

mod claim;
mod delivery;
mod diagnostics;
mod followup;
mod model;
mod notify;
mod retry;

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) use super::latest_completed_review_provider_on_conn;
pub(crate) use super::{
    DispatchReactionRow, ReviewFollowupCard, latest_work_dispatch_thread_pg,
    load_dispatch_context_pg, load_dispatch_reaction_row_pg, load_review_followup_card_pg,
    persist_dispatch_message_target_pg, persist_dispatch_slot_index_pg,
    persist_dispatch_thread_id_pg, review_followup_already_resolved_pg,
};

pub(crate) use claim::{
    mark_dispatch_outbox_claimed_pg, select_pending_dispatch_outbox_claim_candidates_pg,
};
pub(crate) use delivery::{
    dispatch_notify_delivery_suppressed_pg, mark_dispatch_dispatched_pg, mark_outbox_done_pg,
    mark_outbox_failed_pg,
};
pub(crate) use diagnostics::record_routing_diagnostics_pg;
pub(crate) use followup::{
    clear_all_dispatch_threads_pg, load_card_status_pg, load_completed_dispatch_info_pg,
};
#[cfg(test)]
pub(crate) use model::DispatchOutboxClaimCandidate;
pub(crate) use model::{CompletedDispatchInfo, DispatchOutboxRow};
pub(crate) use notify::requeue_dispatch_notify_pg;
pub(crate) use retry::schedule_outbox_retry_pg;
