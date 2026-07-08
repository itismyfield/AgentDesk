//! #3089 S2 single-message status-panel footer helpers for the turn bridge.

use super::*;

pub(super) fn make_owner(
    user_msg_id: Option<MessageId>,
    started_at_unix: i64,
) -> super::footer_view_reconciler::CompletionFooterOwner {
    super::footer_view_reconciler::CompletionFooterOwner::new(
        user_msg_id.map(|id| id.get()).unwrap_or(0),
        started_at_unix,
    )
}

pub(super) fn make_owner_now(
    user_msg_id: Option<MessageId>,
) -> (i64, super::footer_view_reconciler::CompletionFooterOwner) {
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

/// #3813 Phase 2: should the status-panel / footer edit be DEFERRED this loop
/// pass because the turn's opening answer body has not reached Discord yet?
///
/// The status-panel edit and the #4006 first-output fast-lane streaming edit
/// share the same per-channel Discord rate lane (`discord_io` 1s `min_gap`).
/// When a v2 panel is dirty from turn start it is eligible immediately, so on
/// the very first loop pass it can consume that lane BEFORE the fast-lane first
/// answer, pushing the opening answer back by up to the `min_gap` and eroding
/// the fast lane's benefit. This predicate holds the panel edit back for exactly
/// that window — while the first answer has NOT been relayed
/// (`!first_answer_relayed`) AND there is un-relayed answer body pending
/// (`first_answer_text_pending`).
///
/// It deliberately does NOT gate on `!first_answer_relayed` ALONE. A tool-only
/// turn (or any turn whose bridge never relays assistant body — e.g. a
/// watcher/standby-owned relay, where `response_sent_offset` tracks the response
/// length so nothing stays pending) leaves `first_answer_relayed` false for the
/// whole turn; a bare gate would then suppress the live panel for the entire
/// turn (#3477 regression). Requiring `first_answer_text_pending` too means the
/// deferral only bites while an opening answer body is genuinely competing for
/// the lane. The caller leaves `status_panel_dirty` set across the skip, so the
/// panel renders on the next interval once the first answer has been relayed —
/// coalesced by at most one interval, never dropped.
pub(super) fn status_panel_edit_defer_for_first_answer(
    first_answer_relayed: bool,
    first_answer_text_pending: bool,
) -> bool {
    !first_answer_relayed && first_answer_text_pending
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
    two_message_panel_enabled: bool,
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
    status_panel_generation: &mut u64,
    response_sent_offset: usize,
    full_response: &str,
    status_panel_dirty: &mut bool,
    _shared: &Arc<SharedData>,
    _provider: &crate::services::provider::ProviderKind,
) {
    // #3805 P2 (PR-B): flag ON → create the status panel as a NEW message BELOW
    // the answer (answer-first layout). The ON path is fully mutually exclusive
    // with the OFF panel-above swap below: it either creates the two-message
    // panel or does nothing, so the default-OFF path stays byte-identical.
    if two_message_panel_enabled {
        if super::two_message_panel::bridge_should_create_two_message_status_panel(
            two_message_panel_enabled,
            single_message_panel_footer_mode,
            status_panel_v2_enabled,
            *status_panel_msg_id,
            *current_msg_id,
        ) {
            super::two_message_panel::create_bridge_two_message_status_panel_below_answer(
                gateway,
                channel_id,
                initial_indicator,
                *current_msg_id,
                status_panel_msg_id,
                inflight_state,
                status_panel_generation,
                status_panel_dirty,
            )
            .await;
        }
        return;
    }

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
    super::footer_view_reconciler::note_footer_suppressed_for_tui_mirror(
        super::footer_view_reconciler::FooterViewWriter::bridge(shared),
        channel_id,
        Some(terminal_msg_id),
        provider,
        terminal_text,
        "turn_bridge_tui_mirror",
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn complete_bridge_single_message_completion_footer(
    shared: &SharedData,
    channel_id: ChannelId,
    terminal_msg_id: MessageId,
    owner: super::footer_view_reconciler::CompletionFooterOwner,
    provider: &ProviderKind,
    _started_at_unix: i64,
    terminal_text: &str,
    indicator: &str,
    background: bool,
    background_agent_pending: bool,
) -> bool {
    super::footer_view_reconciler::note_turn_completed_footer(
        super::footer_view_reconciler::FooterViewWriter::bridge(shared),
        channel_id,
        Some(terminal_msg_id),
        owner,
        provider,
        terminal_text,
        indicator,
        background,
        background_agent_pending,
        "turn_bridge_single_message_footer",
    )
    .await
}

pub(super) async fn supersede_bridge_footer(
    shared: &SharedData,
    channel_id: ChannelId,
    owner: super::footer_view_reconciler::CompletionFooterOwner,
) -> bool {
    super::footer_view_reconciler::note_footer_superseded(
        super::footer_view_reconciler::FooterViewWriter::bridge(shared),
        channel_id,
        owner,
        "turn_bridge_supersede",
    )
    .await
}

pub(super) async fn refresh_bridge_footer(
    shared: &SharedData,
    channel_id: ChannelId,
    owner: super::footer_view_reconciler::CompletionFooterOwner,
    indicator: &str,
) -> bool {
    super::footer_view_reconciler::note_background_refresh_due(
        super::footer_view_reconciler::FooterViewWriter::bridge(shared),
        channel_id,
        Some(owner),
        indicator,
        "turn_bridge_refresh",
    )
    .await
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
    this_turn_status_panel_generation: u64,
    tmux_session_name: Option<&str>,
) -> bool {
    complete_bridge_terminal_footer_or_status_panel_with_sniffer(
        shared,
        gateway,
        channel_id,
        current_msg_id,
        user_msg_id,
        status_panel_msg_id,
        provider,
        started_at_unix,
        last_status_panel_text,
        single_message_panel_footer_mode,
        is_external_input_tui_direct,
        terminal_text,
        indicator,
        this_turn_status_panel_generation,
        tmux_session_name.map(str::to_string),
        |tmux_session_name| async move {
            // #4353: `super::super::tmux` is cfg(unix). No tmux pane means nothing
            // can be pending in one.
            #[cfg(unix)]
            {
                super::super::tmux::sniff_background_agent_pending_for_completion(
                    tmux_session_name.as_deref(),
                )
                .await
            }
            #[cfg(not(unix))]
            {
                let _ = tmux_session_name;
                false
            }
        },
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn complete_bridge_terminal_footer_or_status_panel_with_sniffer<
    G,
    S,
    SniffFuture,
>(
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
    this_turn_status_panel_generation: u64,
    tmux_session_name: Option<String>,
    sniff_background_agent_pending: S,
) -> bool
where
    G: TurnGateway + ?Sized,
    S: FnOnce(Option<String>) -> SniffFuture,
    SniffFuture: std::future::Future<Output = bool>,
{
    let this_turn_user_msg_id = user_msg_id.map(|id| id.get()).unwrap_or(0);
    // #3805 P2: a completion edit is skipped when EITHER a different real turn
    // now owns this panel (identity aliasing, unchanged) OR — under the
    // two-message path — a NEWER panel epoch has superseded this stale edit for
    // the SAME owned panel. On the default-OFF path every generation is 0, so
    // the generation term is inert and this stays byte-identical.
    let (aliases_newer_turn, generation_superseded) =
        match super::inflight::load_inflight_state(provider, channel_id.get()) {
            Some(on_disk) => {
                let identity_alias =
                    super::status_panel::status_panel_completion_edit_aliases_newer_turn(
                        this_turn_user_msg_id,
                        status_panel_msg_id,
                        on_disk.user_msg_id,
                        on_disk.status_message_id,
                    );
                let panel_owned_on_disk = match status_panel_msg_id {
                    Some(id) if !is_synthetic_headless_message_id(id) => {
                        on_disk.status_message_id == Some(id.get())
                    }
                    _ => false,
                };
                let generation_superseded =
                    super::two_message_panel::two_message_status_edit_generation_is_stale(
                        this_turn_status_panel_generation,
                        panel_owned_on_disk,
                        on_disk.status_panel_generation,
                    );
                (identity_alias, generation_superseded)
            }
            None => (false, false),
        };
    if (aliases_newer_turn || generation_superseded) && !single_message_panel_footer_mode {
        tracing::debug!(
            "[turn_bridge] skipping status-panel-v2 completion edit of msg {:?} in channel {}: a newer turn now owns the panel (this turn user_msg_id {})",
            status_panel_msg_id,
            channel_id,
            this_turn_user_msg_id
        );
        return true;
    }
    let background_agent_pending = sniff_background_agent_pending(tmux_session_name).await;
    if single_message_panel_footer_mode {
        let owner = super::footer_view_reconciler::CompletionFooterOwner::new(
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
                        background_agent_pending,
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
        background_agent_pending,
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

    struct RuntimeRootGuard {
        previous: Option<std::ffi::OsString>,
        _root: tempfile::TempDir,
    }

    impl RuntimeRootGuard {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("runtime root");
            let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
            unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };
            Self {
                previous,
                _root: root,
            }
        }
    }

    impl Drop for RuntimeRootGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    fn isolate_agentdesk_runtime_root() -> (std::sync::MutexGuard<'static, ()>, RuntimeRootGuard) {
        let lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = RuntimeRootGuard::new();
        (lock, root)
    }

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

    #[tokio::test]
    async fn bridge_single_message_completion_footer_emits_background_agent_pending_payload() {
        let (_env_lock, _runtime_root) = isolate_agentdesk_runtime_root();
        let shared = super::super::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_047_201);
        let provider = ProviderKind::Claude;
        let owner =
            super::footer_view_reconciler::CompletionFooterOwner::new(4_047_202, 1_700_000_000);

        let _ = complete_bridge_single_message_completion_footer(
            shared.as_ref(),
            channel_id,
            MessageId::new(4_047_203),
            owner,
            &provider,
            1_700_000_000,
            "Final answer",
            "⠸",
            false,
            true,
        )
        .await;

        let rendered = shared
            .ui
            .placeholder_live_events
            .render_completion_footer(channel_id, &provider, "⠸");
        let block = rendered.block.expect("background-agent pending footer");

        assert!(rendered.has_unfinished_entries);
        assert!(block.contains("Background agents"));
        assert!(block.contains("Waiting for background agents ⠸"));
    }

    #[tokio::test]
    async fn bridge_single_message_completion_footer_producer_threads_sniffed_background_agent_pending()
     {
        let (_env_lock, _runtime_root) = isolate_agentdesk_runtime_root();
        for (pending, channel_raw) in [(true, 4_047_211), (false, 4_047_212)] {
            let shared = super::super::make_shared_data_for_tests();
            let gateway = crate::services::discord::gateway::HeadlessGateway;
            let channel_id = ChannelId::new(channel_raw);
            let provider = ProviderKind::Claude;
            let observed_tmux_session = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sniffer_observed_tmux_session = observed_tmux_session.clone();
            let mut last_status_panel_text = String::new();

            let _committed = complete_bridge_terminal_footer_or_status_panel_with_sniffer(
                shared.as_ref(),
                &gateway,
                channel_id,
                MessageId::new(channel_raw + 1),
                Some(MessageId::new(channel_raw + 2)),
                None,
                &provider,
                1_700_000_000,
                &mut last_status_panel_text,
                true,
                false,
                Some("Final answer"),
                "⠸",
                0,
                Some("AgentDesk-claude-background-test".to_string()),
                move |tmux_session_name| async move {
                    sniffer_observed_tmux_session
                        .lock()
                        .expect("observed tmux session lock")
                        .push(tmux_session_name);
                    pending
                },
            )
            .await;

            assert_eq!(
                observed_tmux_session
                    .lock()
                    .expect("observed tmux session lock")
                    .as_slice(),
                &[Some("AgentDesk-claude-background-test".to_string())]
            );

            let rendered = shared
                .ui
                .placeholder_live_events
                .render_completion_footer(channel_id, &provider, "⠸");
            let block_has_background_agents = rendered
                .block
                .as_deref()
                .is_some_and(|block| block.contains("Background agents"));

            assert_eq!(rendered.has_unfinished_entries, pending);
            assert_eq!(block_has_background_agents, pending);
        }
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
    fn status_panel_defers_only_while_first_answer_body_pending() {
        // First answer body pending and not yet relayed → defer the panel edit
        // so the #4006 fast lane wins the shared rate lane.
        assert!(status_panel_edit_defer_for_first_answer(false, true));
        // No answer body pending (tool-only turn / watcher-owned relay where the
        // response offset already tracks the length) → never defer. This is the
        // #3477 live-panel guard: `!first_answer_relayed` alone must NOT suppress.
        assert!(!status_panel_edit_defer_for_first_answer(false, false));
        // First answer already relayed → never defer; the normal interval
        // throttle resumes for the rest of the turn.
        assert!(!status_panel_edit_defer_for_first_answer(true, true));
        assert!(!status_panel_edit_defer_for_first_answer(true, false));
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
