//! Session-bound relay display-text derivation, moved verbatim from the sink
//! delivery path. The RAW pre-format body stays in the parent (#4081 delivery
//! fingerprints record RAW, never this display text).

use super::super::formatting;
use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::provider::ProviderKind;
use serenity::model::id::{ChannelId, MessageId};

pub(super) fn session_bound_relay_bodies(
    shared: &super::super::SharedData,
    provider: &ProviderKind,
    delivery: &super::SessionRelayDelivery,
) -> (String, String) {
    let raw_response_text = delivery.response_text.clone();
    let formatted = if shared.ui.status_panel_v2_enabled {
        formatting::format_for_discord_with_status_panel(&raw_response_text, provider)
    } else {
        formatting::format_for_discord_with_provider(&raw_response_text, provider)
    };
    let formatted = if matches!(
        delivery.task_notification_kind.as_ref(),
        Some(TaskNotificationKind::MonitorAutoTurn)
    ) {
        super::super::prepend_monitor_auto_turn_origin(&formatted)
    } else {
        formatted
    };
    let relay_text = super::super::session_banner::with_discord_turn_session_banner_identity_prefix(
        shared,
        ChannelId::new(delivery.channel_id),
        provider,
        delivery.frame_turn_user_msg_id,
        Some(&delivery.frame_turn_started_at),
        delivery.frame_turn_start_offset,
        true,
        formatted,
    );
    (raw_response_text, relay_text)
}

pub(super) fn ssh_direct_prompt_anchor_for_response(
    provider: &ProviderKind,
    tmux_session_name: &str,
    channel_id: u64,
) -> Option<crate::services::tui_prompt_dedupe::TuiPromptAnchor> {
    crate::services::tui_prompt_dedupe::prompt_anchor_for_response(
        provider.as_str(),
        tmux_session_name,
        channel_id,
    )
}

pub(super) fn clear_ssh_direct_prompt_anchor(
    provider: &ProviderKind,
    tmux_session_name: &str,
    anchor: crate::services::tui_prompt_dedupe::TuiPromptAnchor,
) {
    crate::services::tui_prompt_dedupe::clear_prompt_anchor_for_response(
        provider.as_str(),
        tmux_session_name,
        anchor,
    );
}

pub(super) fn prompt_anchor_reference(
    anchor: Option<crate::services::tui_prompt_dedupe::TuiPromptAnchor>,
) -> Option<(ChannelId, MessageId)> {
    anchor.map(|anchor| {
        (
            ChannelId::new(anchor.channel_id),
            MessageId::new(anchor.message_id),
        )
    })
}
