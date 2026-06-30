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

/// #3959: should the #3089 single-message-panel completion footer
/// (`Context … tokens`, `Tasks`, `Subagents`) be SUPPRESSED for this terminal
/// relay?
///
/// The footer is a status surface AgentDesk RENDERS from JSONL `StatusEvent`s
/// (`placeholder_live_events::context_panel`/`completion_footer`) and appends to
/// the final message — it is NOT a tmux pane snapshot. For a Discord-originated
/// turn the user has no terminal, so the footer is their only live status view
/// and stays. For a TUI-direct external-input turn the Discord message is only a
/// MIRROR of what the user is already watching in their Claude Code TUI pane,
/// which renders the very same Context/Tasks/Subagents chrome. Re-appending our
/// reconstruction duplicates that chrome into the relayed prose (issue #3959),
/// so the mirror must carry the assistant prose ALONE.
///
/// Pure + mutation-pinned: the `is_external_input_tui_direct &&` scoping keeps
/// Discord-origin #3089 footers untouched (no blanket strip).
pub(super) fn tui_direct_completion_footer_suppressed(
    single_message_panel_footer_mode: bool,
    is_external_input_tui_direct: bool,
) -> bool {
    single_message_panel_footer_mode && is_external_input_tui_direct
}

/// #3959: deliver the TUI-direct terminal mirror as clean assistant prose with
/// NO completion footer appended (and no footer-refresh target registered).
///
/// `terminal_text` is the JSONL-derived `delivery_response` (already clean prose
/// for the short-replace path, which has rewritten the message content). We run
/// the shared strip with a `None` completion block so any residual live status
/// footer is removed but nothing is re-appended; an already-clean body short-
/// circuits to no edit. A stale registry target (e.g. from a prior turn on the
/// channel) is forgotten so the background footer-refresh loop never re-adds the
/// chrome onto this mirror.
async fn complete_bridge_single_message_terminal_no_footer(
    shared: &SharedData,
    channel_id: ChannelId,
    terminal_msg_id: MessageId,
    provider: &ProviderKind,
    terminal_text: &str,
) -> bool {
    super::single_message_panel::completion_footer_forget_registered_target_if_message(
        channel_id,
        terminal_msg_id,
    );
    let Some(finalized) = super::single_message_panel::finalize_streaming_footer_with_completion(
        terminal_text,
        provider,
        None,
    ) else {
        // Already clean (short-replace rewrote the message to this prose) — the
        // mirror carries the assistant turn with no chrome, nothing to edit.
        return true;
    };
    match edit_bridge_completion_footer(shared, channel_id, terminal_msg_id, &finalized).await {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                "[turn_bridge] #3959 failed to strip TUI-direct mirror footer on message {} in channel {}: {}",
                terminal_msg_id,
                channel_id,
                error
            );
            false
        }
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
    let inflight = crate::services::discord::turn_end_wip_warning::load_matching_inflight_state(
        provider,
        channel_id,
        Some(owner.user_msg_id),
    );
    let _ = crate::services::discord::turn_end_wip_warning::warn_turn_end_wip_with_shared_http(
        shared,
        channel_id,
        inflight.as_ref(),
        "turn_bridge_single_message_footer",
    )
    .await;
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
    is_external_input_tui_direct: bool,
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
                if tui_direct_completion_footer_suppressed(
                    single_message_panel_footer_mode,
                    is_external_input_tui_direct,
                ) {
                    // #3959: TUI-direct mirror — deliver clean prose, no chrome.
                    complete_bridge_single_message_terminal_no_footer(
                        shared,
                        channel_id,
                        current_msg_id,
                        provider,
                        text,
                    )
                    .await
                } else {
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

    // ---- #3959: TUI-direct mirror suppresses the #3089 chrome footer ----
    //
    // The `Context … tokens · auto-compact` / `Tasks` / `Subagents` block the
    // relay appended to TUI-direct mirror messages is NOT a tmux pane snapshot —
    // it is AgentDesk's own single-message-panel completion footer, rendered from
    // JSONL StatusEvents. For a TUI-direct mirror the user already sees that chrome
    // in their terminal, so the relayed body must be the assistant prose ALONE.

    #[test]
    fn tui_direct_completion_footer_suppression_gate_3959() {
        // Scoped: suppressed ONLY for a TUI-direct external-input turn in footer
        // mode. A Discord-origin footer-mode turn keeps the #3089 footer (the user
        // has no terminal mirror), and non-footer-mode is unaffected.
        assert!(tui_direct_completion_footer_suppressed(true, true));
        assert!(!tui_direct_completion_footer_suppressed(true, false));
        assert!(!tui_direct_completion_footer_suppressed(false, true));
        assert!(!tui_direct_completion_footer_suppressed(false, false));
    }

    #[test]
    fn tui_direct_mirror_emits_prose_without_tui_chrome_3959() {
        // The exact #3959 corruption: assistant prose followed by the rendered
        // Context/Tasks/Subagents completion footer. The TUI-direct mirror
        // suppresses the completion block (None), so the relayed body is the
        // assistant turn ALONE — no chrome, no merge, no truncation; while a
        // Discord-origin turn (block present) still carries the footer.
        let prose = "#3955 머지 완료 — 다음 작업 대기.";
        let chrome_block = "Context   📦 526.3k / 1.0M tokens (52%) · auto-compact 60%\n\nTasks\n└ TaskUpdate 4 · 머지 완료\n\nSubagents\n└ general-purpose Investigate #3658 — Agent \"Implement #3886\"";

        let discord_origin =
            super::single_message_panel::compose_completion_footer_text(prose, Some(chrome_block));
        assert!(discord_origin.starts_with(prose));
        assert!(discord_origin.contains("Context   📦"));
        assert!(discord_origin.contains("Tasks"));
        assert!(discord_origin.contains("Subagents"));

        let tui_direct_mirror =
            super::single_message_panel::compose_completion_footer_text(prose, None);
        assert_eq!(tui_direct_mirror, prose);
        assert!(!tui_direct_mirror.contains("Context"));
        assert!(!tui_direct_mirror.contains("tokens"));
        assert!(!tui_direct_mirror.contains("auto-compact"));
        assert!(!tui_direct_mirror.contains("Tasks"));
        assert!(!tui_direct_mirror.contains("Subagents"));
    }

    #[test]
    fn tui_direct_mirror_finalize_strips_live_status_footer_to_prose_3959() {
        // If the streaming placeholder still carries the LIVE status footer at
        // completion (the non-short-replace path), the TUI-direct finalize (block
        // = None) strips it down to the assistant prose with no chrome re-appended.
        let body_with_footer = format!(
            "Final answer\n\n{}",
            super::single_message_panel::compose_footer_status_block("⠸", PANEL)
        );
        let finalized = super::single_message_panel::finalize_streaming_footer_with_completion(
            &body_with_footer,
            &ProviderKind::Claude,
            None,
        )
        .expect("a live status footer must finalize to a stripped edit");
        assert_eq!(finalized, "Final answer");
        assert!(!finalized.contains("Subagents"));
        assert!(!finalized.contains("진행 중"));
    }

    #[test]
    fn tui_direct_mirror_clean_prose_needs_no_redundant_edit_3959() {
        // The short-replace path already rewrote the message to clean prose, so the
        // mirror finalize is a no-op: no chrome, and no redundant Discord edit.
        let prose = "그냥 평범한 응답입니다.";
        assert!(
            super::single_message_panel::finalize_streaming_footer_with_completion(
                prose,
                &ProviderKind::Claude,
                None,
            )
            .is_none()
        );
    }
}
