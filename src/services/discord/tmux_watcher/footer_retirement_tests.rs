use super::*;
use crate::services::discord::formatting;

#[test]
fn retired_watcher_disables_new_surfaces_but_keeps_upgrade_completion() {
    for single in [false, true] {
        for v2 in [false, true] {
            assert!(!footer_mode_enabled(single, v2));
            for present in [false, true] {
                for eligible in [false, true] {
                    assert!(!watcher_should_create_separate_status_panel(
                        single, v2, present, eligible
                    ));
                }
            }
        }
    }
    assert!(watcher_should_complete_separate_status_panel(true));
    assert!(!watcher_should_complete_separate_status_panel(false));
    assert!(watcher_separate_status_panel_enabled(true));
    assert!(!watcher_separate_status_panel_enabled(false));
    assert!(
        !crate::services::discord::turn_bridge::two_message_should_reanchor_panel_on_rollover(
            true, true
        )
    );
}

#[test]
fn retired_watcher_builder_preserves_actual_footer_budget_input() {
    use crate::services::discord::placeholder_live_events::footer_activity_tests::{
        START, six_tool_fixture,
    };
    let channel = ChannelId::new(8_071_816);
    let shared = six_tool_fixture(crate::config::ToolActivityDisplay::Summary, channel);
    assert!(crate::services::discord::single_message_panel_enabled());
    for provider in [ProviderKind::Claude, ProviderKind::Codex] {
        for unit in ["x", "한", "```rust\nlet x = 1;\n```\n\n"] {
            for len in [0, 1, 100, 1700, 3000, 6000] {
                let body = unit.repeat(len);
                let builder = || {
                    build_watcher_single_message_panel_status_block(
                        &shared, channel, &provider, START, "⠸", None, None, &body, None,
                    )
                };
                let block = builder();
                assert_eq!(
                    block,
                    shared
                        .ui
                        .placeholder_live_events
                        .render_footer_status_panel(channel, &provider, START, (&body, "⠸")),
                    "budget input must retain the legacy panel, not a spinner"
                );
                assert!(block.len() > formatting::build_processing_status_block("⠸").len());
                if body.len() <= 100 {
                    assert!(block.contains("fixture0"));
                }
                let text = formatting::build_streaming_placeholder_text(&body, &block);
                if body.is_empty() {
                    assert_eq!(text, "⠋ 계속 처리 중");
                } else {
                    assert!(!text.ends_with(&format!("\n\n{block}")));
                    assert!(!text.contains("fixture0"));
                }
                if !body.is_empty() {
                    assert!(!text.contains("계속 처리 중"));
                }
            }
        }
    }
}
