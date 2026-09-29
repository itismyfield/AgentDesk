//! Watcher terminal arm for channels whose TUI body O posts: Legacy consumes the range with no
//! transport, no lease and no delivery evidence, then clears its own "..." placeholder.

use std::sync::Arc;

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
}

/// Mirrors the delegated-success watermark epilogue; the confirmed end advances later through
/// the watcher's lease-free commit path.
pub(super) async fn consume_delegated_terminal(arm: DelegatedTerminal<'_>) {
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
}
