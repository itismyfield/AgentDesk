use super::*;
use crate::services::discord::placeholder_live_events::status_events_from_tool_use_with_id;

#[test]
fn o_status_only_frame_uses_the_same_summary_and_class_renderer() {
    for mode in [
        crate::config::ToolActivityDisplay::Summary,
        crate::config::ToolActivityDisplay::Class,
    ] {
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        let ui = &mut std::sync::Arc::get_mut(&mut shared)
            .expect("unshared fixture")
            .ui;
        ui.status_panel_v2_enabled = true;
        ui.placeholder_live_events = std::sync::Arc::new(crate::services::discord::placeholder_live_events::PlaceholderLiveEvents::with_tool_activity_display(mode));
        let channel = ChannelId::new(807151);
        ui.placeholder_live_events.push_status_events(
            channel,
            status_events_from_tool_use_with_id(
                "exec_command",
                r#"{"cmd":"cargo test"}"#,
                Some("o-tool"),
            ),
        );
        let provider = ProviderKind::Codex;
        let panel = shared.ui.placeholder_live_events.render_status_panel(
            channel,
            &provider,
            1_700_000_000,
        );
        let frame = status_frame(&shared, channel, &provider, 1_700_000_000, "⠋");
        let expected = crate::services::discord::single_message_panel::compose_footer_status_block(
            "⠋", &panel,
        );
        assert_eq!(frame, "⠋ 계속 처리 중");
        assert!(tool_label(&frame).is_none());
        assert_ne!(frame, expected, "rich renderer output remains budget-only");
        if mode == crate::config::ToolActivityDisplay::Summary {
            assert!(expected.contains("▶ Bash  cargo test"), "{expected}");
        } else {
            assert!(expected.contains("마지막 도구 ([Bash])") && !expected.contains("cargo test"));
        }
    }
}
