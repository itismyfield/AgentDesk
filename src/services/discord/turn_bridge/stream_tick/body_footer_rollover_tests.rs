use super::super::super::*;
use crate::config::ToolActivityDisplay;
use crate::services::discord::placeholder_live_events::footer_activity_tests::{
    START, rollover_splits, six_tool_fixture,
};

#[test]
fn bridge_body_footer_preserves_class_rollover_for_ascii_and_korean() {
    let channel = ChannelId::new(8_071_601);
    let class = six_tool_fixture(ToolActivityDisplay::Class, channel);
    let summary = six_tool_fixture(ToolActivityDisplay::Summary, channel);
    for provider in [ProviderKind::Claude, ProviderKind::Codex] {
        let block = |shared: &SharedData, body: &str| {
            build_bridge_single_message_panel_status_block(
                shared, channel, &provider, START, "⠋", None, None, body,
            )
        };
        for unit in ["x", "한"] {
            for len in (0..=6000).step_by(25) {
                let body = unit.repeat(len);
                let expected = block(&class, &body);
                let actual = block(&summary, &body);
                let plan = |footer: &str| {
                    rollover_splits(&body, |tail, _| {
                        crate::services::discord::formatting::plan_streaming_rollover(tail, footer)
                            .map(|plan| plan.split_at)
                    })
                };
                assert_eq!(plan(&actual), plan(&expected), "{provider:?} {unit} {len}");
                if body.is_empty() {
                    assert!(
                        actual.contains("도구 6") && actual.contains("fixture0"),
                        "{actual}"
                    );
                }
            }
        }
        assert_eq!(block(&summary, "body"), block(&class, "body"));
        let panel = summary
            .ui
            .placeholder_live_events
            .render_status_panel(channel, &provider, START);
        assert!(
            panel.contains("도구 6") && panel.contains("fixture0"),
            "{panel}"
        );
    }
}
