//! #3089 S2 single-message status-panel footer helpers for the turn bridge.

use super::*;

pub(super) fn bridge_single_message_panel_footer_enabled(status_panel_v2_enabled: bool) -> bool {
    super::single_message_panel::footer_mode_enabled(
        crate::services::discord::single_message_panel_enabled(),
        status_panel_v2_enabled,
    )
}

pub(super) fn bridge_separate_status_panel_enabled(status_panel_v2_enabled: bool) -> bool {
    super::single_message_panel::separate_status_panel_enabled_for_flags(
        crate::services::discord::single_message_panel_enabled(),
        status_panel_v2_enabled,
    )
}

pub(super) fn bridge_status_panel_dirty_should_edit_separate_panel(
    status_panel_dirty: bool,
    single_message_panel_footer_mode: bool,
) -> bool {
    super::single_message_panel::live_events_dirty_should_force_status_update(
        status_panel_dirty,
        single_message_panel_footer_mode,
    )
}

#[cfg(test)]
fn bridge_status_panel_msg_id_for_footer_mode(
    single_message_panel_footer_mode: bool,
    status_panel_msg_id: Option<MessageId>,
) -> Option<MessageId> {
    if single_message_panel_footer_mode {
        None
    } else {
        status_panel_msg_id
    }
}

pub(super) fn bridge_should_create_separate_status_panel(
    single_message_panel_footer_mode: bool,
    status_panel_v2_enabled: bool,
    status_panel_msg_id: Option<MessageId>,
    current_msg_id: MessageId,
) -> bool {
    !single_message_panel_footer_mode
        && status_panel_v2_enabled
        && (status_panel_msg_id.is_none() || status_panel_msg_id == Some(current_msg_id))
}

pub(super) fn bridge_should_complete_separate_status_panel(status_panel_v2_enabled: bool) -> bool {
    bridge_separate_status_panel_enabled(status_panel_v2_enabled)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn maybe_create_bridge_separate_status_panel_response<G: TurnGateway + ?Sized>(
    single_message_panel_footer_mode: bool,
    status_panel_v2_enabled: bool,
    gateway: &G,
    channel_id: ChannelId,
    initial_indicator: &str,
    current_msg_id: &mut MessageId,
    status_panel_msg_id: &mut Option<MessageId>,
    bridge_created_response_placeholder_msg_id: &mut Option<MessageId>,
    last_edit_text: &mut String,
    inflight_state: &mut InflightTurnState,
    response_sent_offset: usize,
    full_response: &str,
    status_panel_dirty: &mut bool,
) {
    if !bridge_should_create_separate_status_panel(
        single_message_panel_footer_mode,
        status_panel_v2_enabled,
        *status_panel_msg_id,
        *current_msg_id,
    ) {
        return;
    }

    let response_placeholder = super::formatting::build_processing_status_block(initial_indicator);
    match gateway
        .send_message(channel_id, &response_placeholder)
        .await
    {
        Ok(response_msg_id) => {
            if is_synthetic_headless_message_id(*current_msg_id) {
                *status_panel_msg_id = None;
                inflight_state.status_message_id = None;
            } else {
                *status_panel_msg_id = Some(*current_msg_id);
                inflight_state.status_message_id = Some(current_msg_id.get());
            }
            *current_msg_id = response_msg_id;
            *bridge_created_response_placeholder_msg_id = Some(response_msg_id);
            *last_edit_text = response_placeholder.to_string();
            inflight_state.current_msg_id = current_msg_id.get();
            inflight_state.current_msg_len = last_edit_text.len();
            inflight_state.response_sent_offset = response_sent_offset;
            inflight_state.full_response = full_response.to_string();
        }
        Err(error) => {
            tracing::warn!(
                "[turn_bridge] failed to create status-panel-v2 response message in channel {}: {}",
                channel_id,
                error
            );
            *status_panel_dirty = false;
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_bridge_single_message_panel_status_block(
    shared: &SharedData,
    channel_id: ChannelId,
    provider: &ProviderKind,
    started_at_unix: i64,
    indicator: &str,
    prev_tool_status: Option<&str>,
    current_tool_line: Option<&str>,
    full_response: &str,
) -> String {
    if bridge_single_message_panel_footer_enabled(shared.status_panel_v2_enabled) {
        let panel_text = shared.placeholder_live_events.render_status_panel(
            channel_id,
            provider,
            started_at_unix,
        );
        return super::single_message_panel::compose_footer_status_block(indicator, &panel_text);
    }
    if shared.status_panel_v2_enabled {
        super::formatting::build_processing_status_block(indicator)
    } else {
        super::formatting::build_placeholder_status_block(
            indicator,
            prev_tool_status,
            current_tool_line,
            full_response,
        )
    }
}

pub(super) fn finalize_bridge_streaming_footer(
    single_message_panel_footer_mode: bool,
    last_edit_text: &str,
    provider: &ProviderKind,
) -> Option<String> {
    if single_message_panel_footer_mode {
        super::single_message_panel::finalize_streaming_footer(last_edit_text, provider)
    } else {
        super::formatting::finalize_stale_streaming_footer(last_edit_text, provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::DISCORD_MSG_LIMIT;

    const PANEL: &str = "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";

    #[test]
    fn bridge_footer_mode_requires_both_flags() {
        assert!(super::single_message_panel::footer_mode_enabled(true, true));
        assert!(!super::single_message_panel::footer_mode_enabled(
            true, false
        ));
        assert!(!super::single_message_panel::footer_mode_enabled(
            false, true
        ));
    }

    #[test]
    fn bridge_footer_streaming_edit_text_includes_panel_footer() {
        let status_block = super::single_message_panel::compose_footer_status_block("⠸", PANEL);
        let rendered = super::super::build_turn_bridge_streaming_edit_text(
            true,
            "Bridge body",
            &status_block,
            &ProviderKind::Claude,
        );

        assert!(rendered.starts_with("Bridge body\n\n⠸ 진행 중 — Claude"));
        assert!(!rendered.contains("계속 처리 중"));
        assert!(!rendered.contains('🟢'));
        assert!(rendered.contains("Subagents\n└ review inspect"));
    }

    #[test]
    fn bridge_footer_disables_separate_panel_creation_and_binding() {
        let footer_mode = super::single_message_panel::footer_mode_enabled(true, true);
        let current = MessageId::new(7);

        assert!(!bridge_should_create_separate_status_panel(
            footer_mode,
            true,
            None,
            current,
        ));
        assert_eq!(
            bridge_status_panel_msg_id_for_footer_mode(footer_mode, Some(MessageId::new(42))),
            None,
        );
    }

    #[test]
    fn bridge_terminal_footer_strips_panel_block() {
        let rendered = format!(
            "Final answer\n\n{}",
            super::single_message_panel::compose_footer_status_block("⠸", PANEL)
        );
        let finalized = finalize_bridge_streaming_footer(true, &rendered, &ProviderKind::Claude)
            .expect("panel footer should strip at terminal reconciliation");

        assert_eq!(finalized, "Final answer");
        assert!(!finalized.contains("계속 처리 중"));
        assert!(!finalized.contains("Subagents"));
    }

    #[test]
    fn bridge_footer_only_dirty_does_not_force_separate_panel_edit() {
        assert!(!bridge_status_panel_dirty_should_edit_separate_panel(
            true, true,
        ));
        assert!(bridge_status_panel_dirty_should_edit_separate_panel(
            true, false,
        ));
    }

    #[test]
    fn bridge_pathological_panel_stays_within_discord_limit() {
        let huge_panel = format!(
            "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n{}",
            "└ reviewer ".repeat(1_000)
        );
        let status_block =
            super::single_message_panel::compose_footer_status_block("⠸", &huge_panel);
        let rendered = super::formatting::build_streaming_placeholder_text("body", &status_block);

        assert!(rendered.len() <= DISCORD_MSG_LIMIT);
        assert!(rendered.contains("\n\n"));
    }
}
