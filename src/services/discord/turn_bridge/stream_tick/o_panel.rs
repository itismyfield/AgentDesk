//! When O posts a channel's body, the bridge placeholder is only the live status panel: status
//! frames edited under its own id while the turn runs, wherever O's posts land.

use super::super::*;

#[cfg(test)]
#[path = "body_footer_rollover_tests.rs"]
mod body_footer_rollover_tests;

/// The status-only frame: spinner, last tool and turn times, with no body.
pub(super) fn status_frame(
    shared: &SharedData,
    channel_id: ChannelId,
    provider: &ProviderKind,
    started_at_unix: i64,
    indicator: &str,
) -> String {
    let block = build_bridge_single_message_panel_status_block(
        shared,
        channel_id,
        provider,
        started_at_unix,
        indicator,
        None,
        None,
        "",
    );
    build_turn_bridge_streaming_edit_text(shared.ui.status_panel_v2_enabled, "", &block, provider)
}

/// The frame's last-tool label without its spinner; None before the turn's first tool.
fn tool_label(frame: &str) -> Option<&str> {
    frame.lines().find_map(|line| {
        let line = line.trim().strip_prefix("-# ").unwrap_or(line.trim());
        let generated = line.strip_prefix("🔧 ").or_else(|| {
            let (marker, label) = line.split_once(' ')?;
            crate::services::discord::single_message_panel::SINGLE_MESSAGE_PANEL_SPINNER_FRAMES
                .contains(&marker)
                .then_some(label)
        });
        let label = generated.unwrap_or(line);
        if label.starts_with("마지막 도구 (") {
            return (label != "마지막 도구 (아직 없음)").then_some(label);
        }
        generated.filter(|label| label.starts_with("도구 "))
    })
}

/// Edits the bound panel in place when due; true when the edit was written.
pub(super) async fn refresh_o_status_panel(
    gateway: &dyn TurnGateway,
    (channel_id, panel, pending): (ChannelId, MessageId, bool),
    inflight_state: &mut InflightTurnState,
    frame: String,
    (edit_due, done): (bool, bool),
    last_edit_text: &mut String,
) -> bool {
    if durable_current_msg_id_from_detached(panel) == 0 || pending {
        return false;
    }
    let unshown = tool_label(&frame).is_some_and(|tool| !last_edit_text.contains(tool));
    let changed = super::super::super::single_message_panel::streaming_footer_text_changed(
        true,
        last_edit_text,
        &frame,
    );
    // The panel's first tool, or one still unshown on the last tick, goes out at once.
    let first = tool_label(last_edit_text).is_none();
    if !(unshown && (first || done) || edit_due && !done)
        || !changed
        || TurnGateway::edit_message(gateway, channel_id, panel, &frame)
            .await
            .is_err()
    {
        return false;
    }
    inflight_state.current_msg_len = frame.len();
    *last_edit_text = frame;
    true
}

#[cfg(test)]
#[path = "o_tool_activity_tests.rs"]
mod tool_activity_tests;

#[cfg(test)]
mod label_tests {
    use super::tool_label;

    #[test]
    fn accuracy_tool_label_only_matches_generated_line_prefixes() {
        for frame in [
            "Plan\n- [ ] 도구 설정 확인",
            "-# ▶ Task  마지막 도구 (가짜)",
            "설명: 도구 설정",
            "-# 마지막 도구 (아직 없음)",
        ] {
            assert!(tool_label(frame).is_none(), "{frame}");
        }
        assert_eq!(
            tool_label("⠋ 처리 중\n-# 🔧 도구 2 · 실행 1 · 실패 0"),
            Some("도구 2 · 실행 1 · 실패 0")
        );
        assert_eq!(
            tool_label("-# 마지막 도구 ([Bash])"),
            Some("마지막 도구 ([Bash])")
        );
    }
}
