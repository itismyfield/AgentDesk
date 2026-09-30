//! Watcher terminal arms by O ownership. On O's channel Legacy promotes any task card, consumes the
//! range with no body transport, lease or evidence and clears its "..."; a Legacy send claims first.

use std::sync::Arc;

use super::task_response_authority::PrepareWatcherTaskResponseError;
use super::*;

use crate::services::discord::inflight::{InflightTurnIdentity, InflightTurnState};

pub(super) struct DelegatedTerminal<'a> {
    pub(super) http: &'a Arc<serenity::Http>,
    pub(super) shared: &'a Arc<SharedData>,
    pub(super) provider: &'a ProviderKind,
    pub(super) channel_id: ChannelId,
    pub(super) tmux_session_name: &'a str,
    pub(super) placeholder_msg_id: Option<MessageId>,
    pub(super) inflight_before_relay: Option<&'a InflightTurnState>,
    pub(super) inflight_identity_before_relay: Option<&'a InflightTurnIdentity>,
    pub(super) consumed_end: u64,
    pub(super) response_sent_offset: usize,
    pub(super) last_edit_text: &'a str,
    pub(super) turn_data_start_offset: u64,
    pub(super) observed_generation_mtime_ns: &'a mut Option<i64>,
    pub(super) task_card:
        Option<&'a crate::services::discord::task_notification_delivery::TaskNotificationContext>,
}

/// Mirrors the delegated-success watermark epilogue; the confirmed end advances later through
/// the watcher's lease-free commit path.
pub(super) async fn consume_delegated_terminal(
    arm: DelegatedTerminal<'_>,
) -> Result<(), PrepareWatcherTaskResponseError> {
    if let Some(context) = arm.task_card {
        super::task_response_authority::promote_delegated_task_card(
            arm.http,
            arm.shared,
            arm.provider,
            arm.channel_id,
            arm.tmux_session_name,
            context,
        )
        .await?;
    }
    let generation_mtime_ns = read_generation_file_mtime_ns(arm.tmux_session_name);
    *arm.observed_generation_mtime_ns = Some(generation_mtime_ns);
    if let Some(msg_id) = arm.placeholder_msg_id {
        let _ = delete_terminal_placeholder_unless_delivered(
            arm.http,
            arm.channel_id,
            arm.shared,
            arm.provider,
            arm.tmux_session_name,
            msg_id,
            arm.inflight_before_relay,
            Some((arm.turn_data_start_offset, arm.consumed_end)),
            arm.response_sent_offset,
            arm.last_edit_text,
            false,
            "watcher_o_delegated_cleanup",
        )
        .await;
    }
    crate::services::observability::watcher_latency::record_first_relay(arm.channel_id.get());
    if let Some(identity) = arm.inflight_identity_before_relay {
        let _ = crate::services::discord::inflight::persist_watcher_relay_watermark_locked(
            arm.provider,
            arm.channel_id.get(),
            identity,
            arm.tmux_session_name,
            crate::services::discord::inflight::WatcherRelayWatermarkPatch {
                last_watcher_relayed_offset: Some(arm.turn_data_start_offset),
                last_watcher_relayed_generation_mtime_ns: Some(generation_mtime_ns),
            },
        );
    }
    clear_provider_overload_retry_state(arm.channel_id);
    Ok(())
}

/// The claim a direct terminal body sends under, taken at each arm's own transport; a task
/// response claims inside its own send instead.
pub(super) fn direct_body_claim(
    claims: bool,
    channel: ChannelId,
    session: &str,
) -> Option<crate::services::tui_o::cutover::BodyClaim<'_>> {
    claims.then(|| crate::services::tui_o::cutover::BodyClaim::tmux(channel.get(), Some(session)))
}

/// Whether O took (or holds) the channel since the watcher's peek, so nothing Legacy sent counts.
pub(super) fn o_took_channel(channel: ChannelId, session: &str) -> bool {
    crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel_tmux(
        channel.get(),
        Some(session),
    ) != Ok(false)
}
