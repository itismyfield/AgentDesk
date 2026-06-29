//! #3089 S1 single-message status-panel footer helpers for the tmux watcher.

use super::*;

pub(super) fn make_owner(
    identity: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
    started_at_unix: i64,
) -> crate::services::discord::single_message_panel::CompletionFooterOwner {
    crate::services::discord::single_message_panel::CompletionFooterOwner::new(
        identity.map(|identity| identity.user_msg_id).unwrap_or(0),
        started_at_unix,
    )
}

pub(super) fn make_owner_now(
    identity: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
) -> (
    i64,
    crate::services::discord::single_message_panel::CompletionFooterOwner,
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

pub(super) fn watcher_completion_footer_should_tick(
    has_registered_target: bool,
    elapsed: std::time::Duration,
    interval: std::time::Duration,
) -> bool {
    has_registered_target && elapsed >= interval
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
    msg_id: serenity::MessageId,
    text: String,
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
        crate::services::discord::single_message_panel::completion_footer_has_registered_target(
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

#[allow(clippy::too_many_arguments)]
pub(super) async fn complete_watcher_single_message_completion_footer(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    terminal_msg_id: Option<serenity::MessageId>,
    owner: crate::services::discord::single_message_panel::CompletionFooterOwner,
    provider: &ProviderKind,
    _started_at_unix: i64,
    terminal_text: &str,
    indicator: &str,
    background: bool,
) -> bool {
    shared.ui.placeholder_live_events.push_status_event(
        channel_id,
        crate::services::agent_protocol::StatusEvent::TurnCompleted { background },
    );
    let rendered = shared
        .ui
        .placeholder_live_events
        .render_completion_footer(channel_id, provider, indicator);
    let Some(msg_id) = terminal_msg_id else {
        return true;
    };
    if let Some(edit) =
        crate::services::discord::single_message_panel::register_completion_footer_target_for_owner(
            channel_id,
            msg_id,
            owner,
            provider,
            chrono::Utc::now().timestamp(),
            terminal_text,
            rendered.block.as_deref(),
            rendered.has_unfinished_entries,
        )
    {
        rate_limit_wait(shared, channel_id).await;
        if let Err(error) = crate::services::discord::http::edit_channel_message(
            http,
            channel_id,
            edit.message_id,
            &edit.text,
        )
        .await
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ watcher: completion footer supersede failed for channel {} msg {}: {error}",
                channel_id.get(),
                edit.message_id.get()
            );
        }
    }
    let Some(finalized) =
        crate::services::discord::single_message_panel::finalize_streaming_footer_with_completion(
            terminal_text,
            provider,
            rendered.block.as_deref(),
        )
    else {
        return true;
    };
    let inflight = crate::services::discord::turn_end_wip_warning::load_matching_inflight_state(
        provider,
        channel_id,
        Some(owner.user_msg_id),
    );
    let _ = crate::services::discord::turn_end_wip_warning::warn_turn_end_wip_with_http(
        http,
        channel_id,
        inflight.as_ref(),
        "tmux_watcher_single_message_footer",
    )
    .await;
    rate_limit_wait(shared, channel_id).await;
    let edited = match crate::services::discord::http::edit_channel_message(
        http, channel_id, msg_id, &finalized,
    )
    .await
    {
        Ok(_) => true,
        Err(error) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ watcher: completion footer edit failed for channel {} msg {}: {error}",
                channel_id.get(),
                msg_id.get()
            );
            false
        }
    };
    let recorded =
        crate::services::discord::single_message_panel::completion_footer_record_committed_text_result_for_owner(
        channel_id,
        msg_id,
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

pub(super) async fn supersede_watcher_footer(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    owner: crate::services::discord::single_message_panel::CompletionFooterOwner,
) -> bool {
    let Some(edit) =
        crate::services::discord::single_message_panel::completion_footer_supersede_registered_target_for_owner(
            channel_id,
            Some(owner),
        )
    else {
        return false;
    };
    rate_limit_wait(shared, channel_id).await;
    match crate::services::discord::http::edit_channel_message(
        http,
        channel_id,
        edit.message_id,
        &edit.text,
    )
    .await
    {
        Ok(_) => true,
        Err(error) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ watcher: completion footer supersede failed for channel {} msg {}: {error}",
                channel_id.get(),
                edit.message_id.get()
            );
            false
        }
    }
}

pub(super) async fn refresh_watcher_registered_completion_footer(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    indicator: &str,
) -> bool {
    let Some(edit) =
        crate::services::discord::single_message_panel::completion_footer_edit_for_registered_target(
            shared.as_ref(),
            channel_id,
            indicator,
        )
    else {
        return false;
    };
    rate_limit_wait(shared, channel_id).await;
    if !crate::services::discord::single_message_panel::completion_footer_edit_still_registered(
        channel_id, &edit,
    ) {
        return false;
    }
    let edited = match crate::services::discord::http::edit_channel_message(
        http,
        channel_id,
        edit.message_id,
        &edit.text,
    )
    .await
    {
        Ok(_) => true,
        Err(error) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ watcher: completion footer refresh failed for channel {} msg {}: {error}",
                channel_id.get(),
                edit.message_id.get()
            );
            false
        }
    };
    crate::services::discord::single_message_panel::completion_footer_record_edit_result_for_edit(
        shared.as_ref(),
        channel_id,
        &edit,
        edited,
    );
    edited
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
    status_panel_completion_user_msg_id: Option<u64>,
    turn_is_external_input_for_session: bool,
) {
    let committed = if single_message_panel_footer_mode {
        let fallback_target =
            placeholder_msg_id.map(|msg_id| WatcherCompletionFooterTerminalTarget {
                msg_id,
                text: last_edit_text.to_string(),
            });
        let target = terminal_target.or(fallback_target);
        let owner = crate::services::discord::single_message_panel::CompletionFooterOwner::new(
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
            target.as_ref().map(|target| target.msg_id),
            owner,
            provider,
            started_at_unix,
            target
                .as_ref()
                .map(|target| target.text.as_str())
                .unwrap_or(""),
            indicator,
            completion_background,
        )
        .await
    } else {
        complete_watcher_status_panel_v2(
            http,
            shared,
            channel_id,
            status_panel_msg_id,
            provider,
            started_at_unix,
            last_status_panel_text,
            completion_background,
            status_panel_completion_user_msg_id,
        )
        .await
    };
    if !turn_is_external_input_for_session {
        return;
    }
    let Some(panel_msg_id) = status_panel_msg_id else {
        return;
    };
    if committed {
        crate::services::discord::status_panel_orphan_store::remove(
            provider,
            &shared.token_hash,
            channel_id.get(),
            panel_msg_id.get(),
        );
    } else {
        enqueue_watcher_status_panel_orphan(shared.as_ref(), provider, channel_id, panel_msg_id);
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ watcher: status panel completion failed for channel {} msg {}; queued durable orphan cleanup",
            channel_id.get(),
            panel_msg_id.get()
        );
    }
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

        assert!(block.starts_with("⠸ 진행 중 — Claude"));
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
            "",
        )
        .expect("footer-bearing status block should force rollover");
        let seed = crate::services::discord::formatting::build_streaming_placeholder_text(
            "",
            &status_block,
        );

        assert!(!plan.frozen_chunk.contains("계속 처리 중"));
        assert!(!plan.frozen_chunk.contains("Subagents"));
        assert!(plan.display_snapshot.ends_with(&footer));
        assert!(seed.starts_with("⠸ 진행 중 — Claude"));
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

    #[test]
    fn completion_footer_tick_requires_registered_unfinished_target() {
        let interval = std::time::Duration::from_secs(5);

        assert!(watcher_completion_footer_should_tick(
            true, interval, interval
        ));
        assert!(!watcher_completion_footer_should_tick(
            false, interval, interval
        ));
        assert!(!watcher_completion_footer_should_tick(
            true,
            std::time::Duration::from_secs(4),
            interval
        ));
    }
}
