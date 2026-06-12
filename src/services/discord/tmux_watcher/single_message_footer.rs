//! #3089 S1 single-message status-panel footer helpers for the tmux watcher.

use super::*;

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
    if watcher_single_message_panel_footer_enabled(shared.status_panel_v2_enabled) {
        let panel_text = shared.placeholder_live_events.render_status_panel(
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

        assert!(block.starts_with("⠸ 계속 처리 중\n🟢 진행 중"));
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
        assert!(seed.starts_with("⠸ 계속 처리 중\n🟢 진행 중"));
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
