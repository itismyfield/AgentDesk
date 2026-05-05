//! Thin dispatch outbox route shim.
//!
//! Outbox orchestration and message shaping live in
//! `crate::services::dispatches::outbox_route`; persistence lives in
//! `crate::db::dispatches::outbox`; queue processing lives in
//! `crate::services::dispatches::outbox_queue`.

pub(crate) use crate::db::dispatches::outbox::requeue_dispatch_notify_pg;
pub(crate) use crate::server::dto::dispatches::DispatchFollowupConfig;
pub(crate) use crate::services::dispatches::outbox_queue::dispatch_outbox_loop;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) use crate::services::dispatches::outbox_queue::{
    OutboxNotifier, process_outbox_batch, process_outbox_batch_with_real_notifier,
};
pub use crate::services::dispatches::outbox_route::resolve_channel_alias_pub;
pub(crate) use crate::services::dispatches::outbox_route::{
    DISPATCH_MESSAGE_HARD_LIMIT, build_minimal_dispatch_message, extract_review_verdict,
    format_dispatch_message, handle_completed_dispatch_followups,
    handle_completed_dispatch_followups_with_config,
    handle_completed_dispatch_followups_with_config_and_transport,
    handle_completed_dispatch_followups_with_pg, prefix_dispatch_message, review_submission_hint,
    review_target_hint, use_counter_model_channel,
};

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use crate::services::dispatches_followup::{
    queue_dispatch_followup_pg, queue_dispatch_followup_sync,
};

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
#[path = "outbox_tests.rs"]
mod tests;
