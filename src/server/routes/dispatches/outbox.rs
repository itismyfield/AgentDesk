//! Thin dispatch outbox route shim.
//!
//! Outbox orchestration and message shaping live in
//! `crate::services::dispatches::outbox_route`; persistence lives in
//! `crate::db::dispatches::outbox`; queue processing lives in
//! `crate::services::dispatches::outbox_queue`.

pub(crate) use crate::db::dispatches::outbox::requeue_dispatch_notify_pg;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) use crate::server::dto::dispatches::DispatchFollowupConfig;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) use crate::services::dispatches::outbox_claiming::claim_pending_dispatch_outbox_batch_pg;
pub(crate) use crate::services::dispatches::outbox_queue::dispatch_outbox_loop;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) use crate::services::dispatches::outbox_queue::{
    OutboxNotifier, process_outbox_batch, process_outbox_batch_with_pg,
    process_outbox_batch_with_real_notifier,
};
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) use crate::services::dispatches::outbox_route::parse_json_value;
pub use crate::services::dispatches::outbox_route::resolve_channel_alias_pub;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) use crate::services::dispatches::outbox_route::{
    DISPATCH_MESSAGE_HARD_LIMIT, extract_review_verdict, handle_completed_dispatch_followups,
    handle_completed_dispatch_followups_with_config,
    handle_completed_dispatch_followups_with_config_and_transport,
    handle_completed_dispatch_followups_with_pg,
};
pub(crate) use crate::services::dispatches::outbox_route::{
    build_minimal_dispatch_message, format_dispatch_message, prefix_dispatch_message,
    review_submission_hint, review_target_hint, use_counter_model_channel,
};

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use crate::services::dispatches_followup::{
    queue_dispatch_followup_pg, queue_dispatch_followup_sync,
};

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
#[path = "outbox_tests.rs"]
mod tests;
