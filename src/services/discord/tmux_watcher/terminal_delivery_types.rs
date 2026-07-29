use serenity::all::MessageId;

use super::{WatcherCompletionFooterTerminalTarget, terminal_long_chunks};
use crate::services::discord::task_notification_delivery as task_delivery;

pub(in crate::services::discord) struct WatcherDirectFallbackLocals<'a> {
    pub(in crate::services::discord) tui_direct_anchor_terminal_body_visible: &'a mut bool,
    pub(in crate::services::discord) placeholder_msg_id: &'a mut Option<MessageId>,
    pub(in crate::services::discord) placeholder_from_restored_inflight: &'a mut bool,
    pub(in crate::services::discord) last_edit_text: &'a mut String,
    pub(in crate::services::discord) watcher_streaming_rollover_frozen_msg_ids:
        &'a mut Vec<MessageId>,
    pub(in crate::services::discord) watcher_terminal_delivery_proof:
        &'a mut Option<terminal_long_chunks::WatcherTerminalDeliveryProof>,
    pub(in crate::services::discord) completion_footer_terminal_target:
        &'a mut Option<WatcherCompletionFooterTerminalTarget>,
    pub(in crate::services::discord) retry_terminal_delivery_from_offset: &'a mut bool,
    pub(in crate::services::discord) terminal_delivery_landed_unproven: &'a mut bool,
    pub(in crate::services::discord) tui_direct_anchor_or_lease_present_for_lifecycle: &'a mut bool,
    pub(in crate::services::discord) watcher_direct_terminal_idle_committed: &'a mut bool,
    pub(in crate::services::discord) last_relayed_offset: &'a mut Option<u64>,
    pub(in crate::services::discord) last_observed_generation_mtime_ns: &'a mut Option<i64>,
    pub(in crate::services::discord) task_response_claim:
        &'a mut Option<task_delivery::ResponseDeliveryClaim>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum WatcherShortReplaceResult {
    Delivered,
    DeliveredFallback {
        edit_error: String,
        replacement_anchor: Option<MessageId>,
    },
    AlreadyCommittedAfterEditFailure {
        edit_error: String,
    },
    /// Transport landed after its immutable source authority became stale.
    LandedStale,
    /// Transport landed, but its durable proof could not be recorded.
    LandedUnrecorded,
    B2Skip,
    PartialFailureRetry,
    Skipped,
}
