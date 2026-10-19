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
    let (mut frames, mut visible, mut planner_ns) = (0usize, 0usize, 0u128);
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
                if !body.is_empty() {
                    frames += 1;
                    visible += usize::from(actual.contains("fixture0"));
                }
                let started = std::time::Instant::now();
                std::hint::black_box((
                    crate::services::discord::formatting::plan_streaming_rollover(&body, &actual),
                    crate::services::discord::formatting::plan_streaming_rollover(&body, &expected),
                ));
                planner_ns += started.elapsed().as_nanos();
                for offset in [0, body.len() / 2, body.len().saturating_sub(unit.len())] {
                    let offset = offset - offset % unit.len();
                    let tail = &body[offset..];
                    let banner = format!(
                        "{}\n(최근 대화 {}개를 읽어들였습니다)\n\n{tail}",
                        "🙂".repeat(100),
                        usize::MAX
                    );
                    for rendered in [tail, banner.as_str()] {
                        let splits = |footer: &str| {
                            rollover_splits(rendered, |rest, _| {
                                crate::services::discord::formatting::plan_streaming_rollover(
                                    rest, footer,
                                )
                                .map(|p| p.split_at)
                            })
                        };
                        assert_eq!(
                            splits(&actual),
                            splits(&expected),
                            "offset={offset} len={len}"
                        );
                    }
                }
                if len <= 500 {
                    assert!(
                        actual.contains("도구 6") && actual.contains("fixture0"),
                        "{actual}"
                    );
                }
            }
        }
        assert!(block(&summary, "body").contains("fixture0"));
        assert!(!block(&class, "body").contains("fixture0"));
        let panel = summary
            .ui
            .placeholder_live_events
            .render_status_panel(channel, &provider, START);
        assert!(
            panel.contains("도구 6") && panel.contains("fixture0"),
            "{panel}"
        );
    }
    println!(
        "FOOTER_VISIBILITY bridge nonempty={frames} summary={visible} planner_pair_mean_ns={}",
        planner_ns / 964
    );
}
