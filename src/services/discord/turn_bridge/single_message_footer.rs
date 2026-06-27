//! #3089 S2 single-message status-panel footer helpers for the turn bridge.

use super::*;

pub(super) fn make_owner(
    user_msg_id: Option<MessageId>,
    started_at_unix: i64,
) -> super::single_message_panel::CompletionFooterOwner {
    super::single_message_panel::CompletionFooterOwner::new(
        user_msg_id.map(|id| id.get()).unwrap_or(0),
        started_at_unix,
    )
}

pub(super) fn make_owner_now(
    user_msg_id: Option<MessageId>,
) -> (i64, super::single_message_panel::CompletionFooterOwner) {
    let started_at_unix = chrono::Utc::now().timestamp();
    (started_at_unix, make_owner(user_msg_id, started_at_unix))
}

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
    _shared: &Arc<SharedData>,
    _provider: &crate::services::provider::ProviderKind,
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
    if bridge_single_message_panel_footer_enabled(shared.ui.status_panel_v2_enabled) {
        let panel_text = shared.ui.placeholder_live_events.render_status_panel(
            channel_id,
            provider,
            started_at_unix,
        );
        return super::single_message_panel::compose_footer_status_block(indicator, &panel_text);
    }
    if shared.ui.status_panel_v2_enabled {
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

async fn edit_bridge_completion_footer(
    shared: &SharedData,
    channel_id: ChannelId,
    msg_id: MessageId,
    text: &str,
) -> Result<(), String> {
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        return Err("no Discord HTTP available for completion footer edit".to_string());
    };
    super::http::edit_channel_message(&http, channel_id, msg_id, text)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn complete_bridge_single_message_completion_footer(
    shared: &SharedData,
    channel_id: ChannelId,
    terminal_msg_id: MessageId,
    owner: super::single_message_panel::CompletionFooterOwner,
    provider: &ProviderKind,
    _started_at_unix: i64,
    terminal_text: &str,
    indicator: &str,
    background: bool,
) -> bool {
    shared
        .ui
        .placeholder_live_events
        .push_status_event(channel_id, StatusEvent::TurnCompleted { background });
    let rendered = shared
        .ui
        .placeholder_live_events
        .render_completion_footer(channel_id, provider, indicator);
    if let Some(edit) = super::single_message_panel::register_completion_footer_target_for_owner(
        channel_id,
        terminal_msg_id,
        owner,
        provider,
        chrono::Utc::now().timestamp(),
        terminal_text,
        rendered.block.as_deref(),
        rendered.has_unfinished_entries,
    ) {
        if let Err(error) =
            edit_bridge_completion_footer(shared, channel_id, edit.message_id, &edit.text).await
        {
            tracing::warn!(
                "[turn_bridge] failed to supersede completion footer message {} in channel {}: {}",
                edit.message_id,
                channel_id,
                error
            );
        }
    }
    let Some(finalized) = super::single_message_panel::finalize_streaming_footer_with_completion(
        terminal_text,
        provider,
        rendered.block.as_deref(),
    ) else {
        return true;
    };
    let edited = match edit_bridge_completion_footer(
        shared,
        channel_id,
        terminal_msg_id,
        &finalized,
    )
    .await
    {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                "[turn_bridge] failed to edit completion footer message {} in channel {}: {}",
                terminal_msg_id,
                channel_id,
                error
            );
            false
        }
    };
    let recorded =
        super::single_message_panel::completion_footer_record_committed_text_result_for_owner(
            channel_id,
            terminal_msg_id,
            owner,
            !rendered.has_unfinished_entries,
            edited,
            &finalized,
            rendered.block.as_deref(),
        );
    // #3391: the finalize edit delivered this render's terminal marks once;
    // evict those slot identities so subsequent footer renders (incl. #3386
    // migration) drop the completed task AND subagent entries.
    if edited && recorded {
        shared
            .ui
            .placeholder_live_events
            .evict_delivered_terminal_footer_tasks(channel_id, &rendered.delivered_terminal_ids);
    }
    edited
}

pub(super) async fn supersede_bridge_footer(
    shared: &SharedData,
    channel_id: ChannelId,
    owner: super::single_message_panel::CompletionFooterOwner,
) -> bool {
    let Some(edit) =
        super::single_message_panel::completion_footer_supersede_registered_target_for_owner(
            channel_id,
            Some(owner),
        )
    else {
        return false;
    };
    match edit_bridge_completion_footer(shared, channel_id, edit.message_id, &edit.text).await {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                "[turn_bridge] failed to supersede completion footer message {} in channel {}: {}",
                edit.message_id,
                channel_id,
                error
            );
            false
        }
    }
}

pub(super) async fn refresh_bridge_footer(
    shared: &SharedData,
    channel_id: ChannelId,
    owner: super::single_message_panel::CompletionFooterOwner,
    indicator: &str,
) -> bool {
    let Some(edit) =
        super::single_message_panel::completion_footer_edit_for_registered_target_for_owner(
            shared, channel_id, owner, indicator,
        )
    else {
        return false;
    };
    if !super::single_message_panel::completion_footer_edit_still_registered(channel_id, &edit) {
        return false;
    }
    let edited = match edit_bridge_completion_footer(
        shared,
        channel_id,
        edit.message_id,
        &edit.text,
    )
    .await
    {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                "[turn_bridge] failed to refresh completion footer message {} in channel {}: {}",
                edit.message_id,
                channel_id,
                error
            );
            false
        }
    };
    super::single_message_panel::completion_footer_record_edit_result_for_edit(
        shared, channel_id, &edit, edited,
    );
    edited
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn complete_bridge_terminal_footer_or_status_panel<G: TurnGateway + ?Sized>(
    shared: &SharedData,
    gateway: &G,
    channel_id: ChannelId,
    current_msg_id: MessageId,
    user_msg_id: Option<MessageId>,
    status_panel_msg_id: Option<MessageId>,
    provider: &ProviderKind,
    started_at_unix: i64,
    last_status_panel_text: &mut String,
    single_message_panel_footer_mode: bool,
    terminal_text: Option<&str>,
    indicator: &str,
) -> bool {
    let this_turn_user_msg_id = user_msg_id.map(|id| id.get()).unwrap_or(0);
    let aliases_newer_turn = match super::inflight::load_inflight_state(provider, channel_id.get())
    {
        Some(on_disk) => super::status_panel::status_panel_completion_edit_aliases_newer_turn(
            this_turn_user_msg_id,
            status_panel_msg_id,
            on_disk.user_msg_id,
            on_disk.status_message_id,
        ),
        None => false,
    };
    if aliases_newer_turn && !single_message_panel_footer_mode {
        tracing::debug!(
            "[turn_bridge] skipping status-panel-v2 completion edit of msg {:?} in channel {}: a newer turn now owns the panel (this turn user_msg_id {})",
            status_panel_msg_id,
            channel_id,
            this_turn_user_msg_id
        );
        return true;
    }
    if single_message_panel_footer_mode {
        let owner = super::single_message_panel::CompletionFooterOwner::new(
            this_turn_user_msg_id,
            started_at_unix,
        );
        return match terminal_text {
            Some(text) => {
                complete_bridge_single_message_completion_footer(
                    shared,
                    channel_id,
                    current_msg_id,
                    owner,
                    provider,
                    started_at_unix,
                    text,
                    indicator,
                    false,
                )
                .await
            }
            None => true,
        };
    }
    super::status_panel::complete_status_panel_v2(
        shared,
        gateway,
        channel_id,
        status_panel_msg_id,
        provider,
        started_at_unix,
        last_status_panel_text,
        false,
        "turn_terminal_delivery",
        this_turn_user_msg_id,
    )
    .await
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
