//! When O posts a channel's body, the bridge placeholder is only the live status panel: status
//! frames while the turn runs, sent again below O's newest post so the panel stays last.

use super::super::*;
use super::guarded_persist::{
    StreamTickCandidateSaveContext, bind_pending_current_message_candidate,
};

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

/// Edits the panel when due, or sends it again below O's newest post and, once bound, hands
/// the old one to the orphan-spinner cleanup (recorded, retried). True when anything was written.
pub(super) async fn refresh_o_status_panel(
    shared: &Arc<SharedData>,
    owner: &Arc<dyn TurnGateway>,
    mut save: StreamTickCandidateSaveContext<'_, dyn TurnGateway>,
    frame: String,
    (edit_due, done): (bool, bool),
    last_edit_text: &mut String,
) -> bool {
    let (gateway, channel_id, panel) = (save.gateway, save.channel_id, *save.current_msg_id);
    let panel_id = durable_current_msg_id_from_detached(panel);
    if panel_id == 0 || save.pending_current_message_candidate.is_some() {
        return false;
    }
    let o_posted = crate::services::tui_o::writer::deliver::last_posted(channel_id.get());
    let unshown = tool_label(&frame).is_some_and(|tool| !last_edit_text.contains(tool));
    // The last tick only shows a tool still unshown, below O's posts like any other tick.
    if done && !unshown {
        return false;
    }
    if o_posted.is_none_or(|posted| posted < panel_id) {
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
        save.inflight_state.current_msg_len = frame.len();
        *last_edit_text = frame;
        return true;
    }
    let next = match TurnGateway::send_message(gateway, channel_id, &frame).await {
        Ok(next) => next,
        Err(error) => {
            tracing::warn!(channel_id = channel_id.get(), %error, "O status panel resend failed");
            return false;
        }
    };
    *save.pending_current_message_candidate = Some(next);
    *save.bridge_created_response_placeholder_msg_id = Some(next);
    *save.current_msg_id = next;
    save.inflight_state.current_msg_id = next.get();
    save.inflight_state.current_msg_len = frame.len();
    *last_edit_text = frame;
    let caller = "turn_bridge::stream_tick::o_status_panel_resend";
    if bind_pending_current_message_candidate(&mut save, caller).await {
        let (shared, gateway) = (Arc::clone(shared), Arc::clone(owner));
        let provider = save.provider;
        cleanup_or_preserve_watcher_orphan_spinner(
            shared,
            provider,
            gateway,
            channel_id,
            panel,
            save.inflight_state,
        )
        .await;
    }
    true
}
