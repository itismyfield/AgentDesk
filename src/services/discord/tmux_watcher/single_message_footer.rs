//! #3089 S1 single-message status-panel footer helpers for the tmux watcher.

use super::*;

pub(super) fn make_owner(
    identity: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
    started_at_unix: i64,
) -> crate::services::discord::footer_view_reconciler::CompletionFooterOwner {
    crate::services::discord::footer_view_reconciler::CompletionFooterOwner::new(
        identity.map(|identity| identity.user_msg_id).unwrap_or(0),
        started_at_unix,
    )
}

pub(super) fn make_owner_now(
    identity: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
) -> (
    i64,
    crate::services::discord::footer_view_reconciler::CompletionFooterOwner,
) {
    let started_at_unix = chrono::Utc::now().timestamp();
    (started_at_unix, make_owner(identity, started_at_unix))
}

pub(super) fn watcher_single_message_panel_footer_enabled(status_panel_v2_enabled: bool) -> bool {
    footer_mode_enabled(
        crate::services::discord::single_message_panel_enabled(),
        status_panel_v2_enabled,
    )
}

fn footer_mode_enabled(single_message_panel_enabled: bool, status_panel_v2_enabled: bool) -> bool {
    crate::services::discord::single_message_panel::footer_mode_enabled(
        single_message_panel_enabled,
        status_panel_v2_enabled,
    )
}

pub(super) fn watcher_separate_status_panel_enabled(status_panel_v2_enabled: bool) -> bool {
    separate_status_panel_enabled_for_flags(
        crate::services::discord::single_message_panel_enabled(),
        status_panel_v2_enabled,
    )
}

fn separate_status_panel_enabled_for_flags(
    single_message_panel_enabled: bool,
    status_panel_v2_enabled: bool,
) -> bool {
    crate::services::discord::single_message_panel::separate_status_panel_enabled_for_flags(
        single_message_panel_enabled,
        status_panel_v2_enabled,
    )
}

pub(super) fn watcher_live_events_dirty_should_force_status_update(
    live_events_dirty: bool,
    single_message_panel_footer_mode: bool,
) -> bool {
    crate::services::discord::single_message_panel::live_events_dirty_should_force_status_update(
        live_events_dirty,
        single_message_panel_footer_mode,
    )
}

#[cfg(test)]
fn watcher_status_panel_msg_id_for_footer_mode(
    single_message_panel_footer_mode: bool,
    status_panel_msg_id: Option<serenity::MessageId>,
) -> Option<serenity::MessageId> {
    if single_message_panel_footer_mode {
        None
    } else {
        status_panel_msg_id
    }
}

pub(super) fn watcher_should_create_separate_status_panel(
    single_message_panel_footer_mode: bool,
    status_panel_v2_enabled: bool,
    status_panel_present: bool,
    panel_eligible_turn: bool,
) -> bool {
    !single_message_panel_footer_mode
        && watcher_should_create_external_input_status_panel(
            status_panel_v2_enabled,
            status_panel_present,
            panel_eligible_turn,
        )
}

pub(super) fn watcher_should_complete_separate_status_panel(status_panel_v2_enabled: bool) -> bool {
    watcher_separate_status_panel_enabled(status_panel_v2_enabled)
}

fn compose_single_message_footer_status_block(indicator: &str, panel_text: &str) -> String {
    crate::services::discord::single_message_panel::compose_footer_status_block(
        indicator, panel_text,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_watcher_single_message_panel_status_block(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    provider: &ProviderKind,
    started_at_unix: i64,
    indicator: &str,
    prev_tool_status: Option<&str>,
    current_tool_line: Option<&str>,
    full_response: &str,
    status_panel_msg_id: Option<serenity::MessageId>,
) -> String {
    if watcher_single_message_panel_footer_enabled(shared.ui.status_panel_v2_enabled) {
        let panel_text = shared.ui.placeholder_live_events.render_status_panel(
            channel_id,
            provider,
            started_at_unix,
        );
        return compose_single_message_footer_status_block(indicator, &panel_text);
    }
    build_watcher_placeholder_status_block(
        shared,
        channel_id,
        indicator,
        prev_tool_status,
        current_tool_line,
        full_response,
        status_panel_msg_id,
    )
}

pub(super) fn finalize_single_message_panel_streaming_footer(
    last_edit_text: &str,
    provider: &ProviderKind,
) -> Option<String> {
    crate::services::discord::single_message_panel::finalize_streaming_footer(
        last_edit_text,
        provider,
    )
}

pub(super) fn finalize_watcher_streaming_footer(
    single_message_panel_footer_mode: bool,
    last_edit_text: &str,
    provider: &ProviderKind,
) -> Option<String> {
    if single_message_panel_footer_mode {
        finalize_single_message_panel_streaming_footer(last_edit_text, provider)
    } else {
        crate::services::discord::formatting::finalize_stale_streaming_footer(
            last_edit_text,
            provider,
        )
    }
}

pub(super) struct WatcherCompletionFooterIdleState {
    tick_at: tokio::time::Instant,
    spin_idx: usize,
}

impl Default for WatcherCompletionFooterIdleState {
    fn default() -> Self {
        Self {
            tick_at: tokio::time::Instant::now(),
            spin_idx: 0,
        }
    }
}

#[derive(Clone)]
pub(super) struct WatcherCompletionFooterTerminalTarget {
    pub(super) msg_id: serenity::MessageId,
    pub(super) text: String,
}

pub(super) fn remember_watcher_completion_footer_terminal_target(
    enabled: bool,
    target: &mut Option<WatcherCompletionFooterTerminalTarget>,
    msg_id: serenity::MessageId,
    text: &str,
) {
    if enabled {
        *target = Some(WatcherCompletionFooterTerminalTarget {
            msg_id,
            text: text.to_string(),
        });
    }
}

pub(super) async fn refresh_watcher_completion_footer_if_due(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    status_panel_v2_enabled: bool,
    state: &mut WatcherCompletionFooterIdleState,
) {
    let has_target =
        crate::services::discord::footer_view_reconciler::completion_footer_has_registered_target(
            channel_id,
        );
    if !watcher_single_message_panel_footer_enabled(status_panel_v2_enabled)
        || !watcher_completion_footer_should_tick(
            has_target,
            state.tick_at.elapsed(),
            crate::services::discord::status_update_interval(),
        )
    {
        return;
    }
    state.tick_at = tokio::time::Instant::now();
    let indicator =
        crate::services::discord::single_message_panel::single_message_panel_spinner_frame(
            state.spin_idx,
        );
    state.spin_idx = state.spin_idx.wrapping_add(1);
    refresh_watcher_registered_completion_footer(http, shared, channel_id, indicator).await;
}

/// #3964: deliver the watcher-relayed TUI mirror as clean assistant prose with NO
/// completion footer (mirror of the bridge's
/// `complete_bridge_single_message_terminal_no_footer`). Forgets the registry
/// target first so `refresh_watcher_completion_footer_if_due` can't re-add chrome,
/// then finalizes with a `None` block (strips any residual live footer; an
/// already-clean body short-circuits to no edit).
async fn complete_watcher_single_message_terminal_no_footer(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    terminal_msg_id: Option<serenity::MessageId>,
    provider: &ProviderKind,
    terminal_text: &str,
) -> bool {
    let Some(msg_id) = terminal_msg_id else {
        return true;
    };
    crate::services::discord::footer_view_reconciler::note_footer_suppressed_for_tui_mirror(
        crate::services::discord::footer_view_reconciler::FooterViewWriter::watcher(shared, http),
        channel_id,
        Some(msg_id),
        provider,
        terminal_text,
        "tmux_watcher_tui_mirror",
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn complete_watcher_single_message_completion_footer(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    terminal_msg_id: Option<serenity::MessageId>,
    owner: crate::services::discord::footer_view_reconciler::CompletionFooterOwner,
    provider: &ProviderKind,
    _started_at_unix: i64,
    terminal_text: &str,
    indicator: &str,
    background: bool,
    background_agent_pending: bool,
) -> bool {
    crate::services::discord::footer_view_reconciler::note_turn_completed_footer(
        crate::services::discord::footer_view_reconciler::FooterViewWriter::watcher(shared, http),
        channel_id,
        terminal_msg_id,
        owner,
        provider,
        terminal_text,
        indicator,
        background,
        background_agent_pending,
        "tmux_watcher_single_message_footer",
    )
    .await
}

pub(super) async fn supersede_watcher_footer(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    owner: crate::services::discord::footer_view_reconciler::CompletionFooterOwner,
) -> bool {
    crate::services::discord::footer_view_reconciler::note_footer_superseded(
        crate::services::discord::footer_view_reconciler::FooterViewWriter::watcher(shared, http),
        channel_id,
        owner,
        "tmux_watcher_supersede",
    )
    .await
}

pub(super) async fn refresh_watcher_registered_completion_footer(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    indicator: &str,
) -> bool {
    crate::services::discord::footer_view_reconciler::note_background_refresh_due(
        crate::services::discord::footer_view_reconciler::FooterViewWriter::watcher(shared, http),
        channel_id,
        None,
        indicator,
        "tmux_watcher_refresh",
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn complete_watcher_terminal_footer_or_status_panel(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    provider: &ProviderKind,
    started_at_unix: i64,
    single_message_panel_footer_mode: bool,
    spin_idx: &mut usize,
    terminal_target: Option<WatcherCompletionFooterTerminalTarget>,
    placeholder_msg_id: Option<serenity::MessageId>,
    last_edit_text: &str,
    status_panel_msg_id: Option<serenity::MessageId>,
    last_status_panel_text: &mut String,
    completion_background: bool,
    background_agent_pending: bool,
    status_panel_completion_user_msg_id: Option<u64>,
    turn_is_external_input_for_session: bool,
    // #3969 root invariant: chokepoint-fresh "this turn is a non-Managed TUI
    // mirror" (`turn_source != Managed`). Suppresses the #3089 footer for the
    // /loop self-paced (ExternalInput) class the stale `turn_is_external_input_for_session`
    // flag misses; never set for a Discord-origin Managed turn.
    turn_is_non_managed_tui_mirror: bool,
    // #3805 P2 (PR-C): a newer panel epoch superseded this stale status-panel
    // completion for the SAME owned panel (computed by the caller against the
    // on-disk row via the shared generation staleness predicate). Only skips the
    // status-panel branch, mirroring the sink completion guard. Inert on the
    // default-OFF path (always false).
    two_message_status_panel_generation_superseded: bool,
) {
    if single_message_panel_footer_mode {
        let fallback_target =
            placeholder_msg_id.map(|msg_id| WatcherCompletionFooterTerminalTarget {
                msg_id,
                text: last_edit_text.to_string(),
            });
        let target = terminal_target.or(fallback_target);
        let target_msg_id = target.as_ref().map(|target| target.msg_id);
        let target_text = target
            .as_ref()
            .map(|target| target.text.as_str())
            .unwrap_or("");
        if watcher_external_input_completion_footer_suppressed(
            single_message_panel_footer_mode,
            turn_is_external_input_for_session,
            completion_background,
            turn_is_non_managed_tui_mirror,
        ) {
            // #3964: watcher-relayed TUI mirror — clean prose, no chrome.
            complete_watcher_single_message_terminal_no_footer(
                http,
                shared,
                channel_id,
                target_msg_id,
                provider,
                target_text,
            )
            .await;
        } else {
            let owner =
                crate::services::discord::footer_view_reconciler::CompletionFooterOwner::new(
                    status_panel_completion_user_msg_id.unwrap_or(0),
                    started_at_unix,
                );
            let indicator =
                crate::services::discord::single_message_panel::single_message_panel_spinner_frame(
                    *spin_idx,
                );
            *spin_idx = (*spin_idx).wrapping_add(1);
            complete_watcher_single_message_completion_footer(
                http,
                shared,
                channel_id,
                target_msg_id,
                owner,
                provider,
                started_at_unix,
                target_text,
                indicator,
                completion_background,
                background_agent_pending,
            )
            .await;
        }
        // Footer mode never owns a separate status panel (`status_panel_msg_id`
        // is None here), so the panel orphan reconcile below is a no-op for it —
        // the prior shared tail returned early via its `let Some(panel_msg_id) =
        // status_panel_msg_id else { return }` guard. Done.
        return;
    }
    // #3805 P2 (PR-C): panel mode — the generation guard (skip a superseded stale
    // edit), the status-panel completion, and the durable orphan reconcile all
    // live in the sibling so the P2 logic stays out of this 700-capped file and
    // shares the sink's staleness predicate (parity).
    complete_watcher_status_panel_v2_with_generation_guard(
        http,
        shared,
        channel_id,
        provider,
        started_at_unix,
        status_panel_msg_id,
        last_status_panel_text,
        completion_background,
        background_agent_pending,
        status_panel_completion_user_msg_id,
        turn_is_external_input_for_session,
        two_message_status_panel_generation_superseded,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::DISCORD_MSG_LIMIT;

    const PANEL: &str = "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";

    #[test]
    fn single_message_panel_footer_mode_requires_both_flags() {
        assert!(footer_mode_enabled(true, true));
        assert!(!footer_mode_enabled(true, false));
        assert!(!footer_mode_enabled(false, true));
    }

    #[test]
    fn single_message_panel_footer_status_block_keeps_spinner_first() {
        let block = compose_single_message_footer_status_block("⠸", PANEL);

        assert!(block.starts_with("-# ⠸ 진행 중 — Claude"));
        assert!(!block.contains("계속 처리 중"));
        assert!(!block.contains('🟢'));
        assert!(block.contains("Subagents\n└ review inspect"));
    }

    #[test]
    fn single_message_panel_footer_disables_separate_panel_creation_and_binding() {
        let footer_mode = footer_mode_enabled(true, true);

        assert!(!watcher_should_create_separate_status_panel(
            footer_mode,
            true,
            false,
            true,
        ));
        assert_eq!(
            watcher_status_panel_msg_id_for_footer_mode(
                footer_mode,
                Some(serenity::MessageId::new(42))
            ),
            None,
        );
    }

    #[test]
    fn single_message_panel_footer_rollover_keeps_panel_in_seed_only() {
        let status_block = compose_single_message_footer_status_block("⠸", PANEL);
        let footer = format!("\n\n{status_block}");
        let current_portion = "x".repeat(DISCORD_MSG_LIMIT);

        let plan = crate::services::discord::formatting::plan_streaming_rollover(
            &current_portion,
            &status_block,
        )
        .expect("footer-bearing status block should force rollover");
        let seed = crate::services::discord::formatting::build_streaming_placeholder_text(
            "",
            &status_block,
        );

        assert!(!plan.frozen_chunk.contains("계속 처리 중"));
        assert!(!plan.frozen_chunk.contains("Subagents"));
        assert!(plan.display_snapshot.ends_with(&footer));
        assert!(seed.starts_with("-# ⠸ 진행 중 — Claude"));
        assert!(!seed.contains("계속 처리 중"));
        assert!(!seed.contains('🟢'));
        assert!(seed.contains("Subagents"));
    }

    #[test]
    fn single_message_panel_terminal_footer_strips_panel_block() {
        let rendered = format!(
            "Final answer\n\n{}",
            compose_single_message_footer_status_block("⠸", PANEL)
        );
        let finalized =
            finalize_single_message_panel_streaming_footer(&rendered, &ProviderKind::Claude)
                .expect("panel footer should strip at terminal reconciliation");

        assert_eq!(finalized, "Final answer");
        assert!(!finalized.contains("계속 처리 중"));
        assert!(!finalized.contains("Subagents"));
    }

    #[test]
    fn single_message_panel_completion_skips_separate_panel_completion() {
        assert!(!separate_status_panel_enabled_for_flags(true, true));
    }

    #[test]
    fn single_message_panel_footer_only_dirty_does_not_force_status_update() {
        assert!(!watcher_live_events_dirty_should_force_status_update(
            true, true,
        ));
        assert!(watcher_live_events_dirty_should_force_status_update(
            true, false,
        ));
    }

    #[test]
    fn single_message_panel_pathological_panel_stays_within_discord_limit() {
        let huge_panel = format!(
            "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n{}",
            "└ reviewer ".repeat(1_000)
        );
        let status_block = compose_single_message_footer_status_block("⠸", &huge_panel);
        let rendered = crate::services::discord::formatting::build_streaming_placeholder_text(
            "body",
            &status_block,
        );

        assert!(rendered.len() <= DISCORD_MSG_LIMIT);
        assert!(rendered.contains("\n\n"));
    }
}
