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

/// Edits the panel when due; a panel above O's newest post is sent again below it, and the old
/// one is deleted once the new one is bound to the turn. Returns whether anything was written.
pub(super) async fn refresh_o_status_panel<G: TurnGateway + ?Sized>(
    mut save: StreamTickCandidateSaveContext<'_, G>,
    frame: String,
    edit_due: bool,
    last_edit_text: &mut String,
) -> bool {
    let (gateway, channel_id, panel) = (save.gateway, save.channel_id, *save.current_msg_id);
    let panel_id = durable_current_msg_id_from_detached(panel);
    if panel_id == 0 || save.pending_current_message_candidate.is_some() {
        return false;
    }
    let o_posted = crate::services::tui_o::writer::deliver::last_posted(channel_id.get());
    if o_posted.is_none_or(|posted| posted < panel_id) {
        let changed = super::super::super::single_message_panel::streaming_footer_text_changed(
            true,
            last_edit_text,
            &frame,
        );
        if !edit_due
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
    if bind_pending_current_message_candidate(&mut save, caller).await
        && let Err(error) = gateway.delete_message(channel_id, panel).await
    {
        tracing::warn!(channel_id = channel_id.get(), panel_id, %error, "old O status panel stays");
    }
    true
}
