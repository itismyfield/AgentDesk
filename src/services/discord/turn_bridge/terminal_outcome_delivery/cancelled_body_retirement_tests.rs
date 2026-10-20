//! Cancellation keeps provider body bytes; its terminal state belongs to the separate summary.

use super::*;
use crate::services::discord::{make_shared_data_for_tests, restart_mode::InflightRestartMode};

#[test]
fn a_cancelled_partial_body_has_no_new_stopped_suffix_in_either_formatter_mode() {
    for v2 in [false, true] {
        let mut shared = make_shared_data_for_tests();
        Arc::get_mut(&mut shared)
            .unwrap()
            .ui
            .status_panel_v2_enabled = v2;
        let provider = ProviderKind::Claude;
        let banner = DiscordTurnSessionBanner::new_with_turn_key(
            &shared,
            ChannelId::new(4091),
            &provider,
            0,
            None,
            None,
        );
        for body in [
            "남은 본문",
            "```rust\nlet x = 1;\n```",
            "literal [Stopped] inside provider text",
        ] {
            let saved = format!("already delivered\n{body}");
            let offset = "already delivered\n".len();
            assert_eq!(
                cancelled_terminal_response(&saved, offset, None, &banner),
                banner.format_discord_body(body),
                "v2={v2}: body must remain the only payload"
            );
        }
    }
}

#[test]
fn an_empty_cancel_can_still_emit_its_standalone_notice_without_placeholder_dots() {
    let shared = make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let banner = DiscordTurnSessionBanner::new_with_turn_key(
        &shared,
        ChannelId::new(4092),
        &provider,
        0,
        None,
        None,
    );
    assert_eq!(
        cancelled_terminal_response("", 0, None, &banner),
        "[Stopped]"
    );
}

#[test]
fn cancellation_retirement_does_not_erase_restart_handoff_failure_guidance() {
    let shared = make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let banner = DiscordTurnSessionBanner::new_with_turn_key(
        &shared,
        ChannelId::new(4093),
        &provider,
        0,
        None,
        None,
    );
    for (mode, notice) in [
        (
            InflightRestartMode::DrainRestart,
            "[Restart handoff incomplete]",
        ),
        (
            InflightRestartMode::HotSwapHandoff,
            "[Runtime handoff incomplete]",
        ),
    ] {
        let actual = cancelled_terminal_response("saved body", 0, Some(mode), &banner);
        assert!(actual.starts_with("saved body"));
        assert!(actual.ends_with(notice));
    }
}
