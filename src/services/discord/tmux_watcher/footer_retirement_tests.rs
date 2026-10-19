use super::*;
use crate::services::discord::{footer_retirement, formatting};

#[test]
fn retired_watcher_disables_new_surfaces_but_keeps_upgrade_completion() {
    assert!(footer_retirement::retired());
    for single in [false, true] {
        for v2 in [false, true] {
            assert!(!footer_mode_enabled(single, v2));
            assert!(!separate_status_panel_enabled_for_flags(single, v2));
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
                let (old_block, old_text, old_plan) = {
                    let _legacy = footer_retirement::legacy_for_test();
                    let block = builder();
                    let text = formatting::build_streaming_placeholder_text(&body, &block);
                    let plan = formatting::plan_streaming_rollover(&body, &block);
                    (block, text, plan)
                };
                let block = builder();
                assert_eq!(
                    block, old_block,
                    "budget input must retain the legacy panel, not a spinner"
                );
                assert!(block.len() > formatting::build_processing_status_block("⠸").len());
                if body.len() <= 100 {
                    assert!(block.contains("fixture0"));
                }
                let plan = formatting::plan_streaming_rollover(&body, &block);
                assert_eq!(
                    plan.as_ref().map(|p| (&p.frozen_chunk, p.split_at)),
                    old_plan.as_ref().map(|p| (&p.frozen_chunk, p.split_at))
                );
                let text = formatting::build_streaming_placeholder_text(&body, &block);
                assert_eq!(
                    text,
                    if body.is_empty() {
                        "⠋ 계속 처리 중"
                    } else {
                        old_text.strip_suffix(&format!("\n\n{block}")).unwrap()
                    }
                );
                if !body.is_empty() {
                    assert!(!text.contains("계속 처리 중"));
                }
            }
        }
    }
}
