//! When O posts a channel's body, the bridge placeholder is only the live status panel: status
//! frames edited under its own id while the turn runs, wherever O's posts land.

use super::super::*;

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
    let label = frame[frame.find("마지막 도구 (")?..].lines().next()?;
    (label != "마지막 도구 (아직 없음)").then_some(label)
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
