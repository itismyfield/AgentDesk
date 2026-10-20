//! Guarded mailbox stops retain their captured actor and reader until the same native turn exits.

use super::turn_bridge::NativeStop;
use super::*;

async fn guarded_cancel(
    shared: &SharedData,
    channel: ChannelId,
    message: Option<MessageId>,
    reason: &str,
) -> CancelActiveTurnResult {
    let Some(handle) = shared.mailbox_peek(channel) else {
        return turn_bridge::empty_native_stop();
    };
    let Ok(snapshot) = handle.try_snapshot().await else {
        return turn_bridge::empty_native_stop();
    };
    if message.is_some() && snapshot.active_user_message_id != message {
        return turn_bridge::empty_native_stop();
    }
    let Some(token) = snapshot.cancel_token else {
        return turn_bridge::empty_native_stop();
    };
    let session = token.tmux_session_name().or_else(|| {
        inflight::load_inflight_state_read_only(&shared.provider, channel.get())
            .and_then(|row| row.tmux_session_name)
    });
    if let Some(session) = session.as_deref()
        && let Some(stop) =
            NativeStop::begin(shared, &shared.provider, channel, &token, session, reason).await
    {
        stop.notice(shared).await;
        return stop.finish().await;
    }
    let result = handle
        .cancel_active_turn_if_current_with_reason(token, reason.into())
        .await;
    if result.token.is_some() {
        record_turn_stop_tombstone(channel, session.as_deref(), reason).await;
    }
    result
}

pub(super) async fn mailbox_cancel_active_turn(
    shared: &SharedData,
    channel_id: ChannelId,
) -> CancelActiveTurnResult {
    // Session restart keeps its existing raw cancellation and reset contract.
    let reason = "mailbox_cancel_active_turn";
    let session = shared
        .tmux_watchers
        .channel_binding(&channel_id)
        .map(|binding| binding.tmux_session_name)
        .or_else(|| infer_inflight_tmux_session_for_channel(channel_id));
    let result = shared
        .mailbox(channel_id)
        .cancel_active_turn_with_reason(reason.into())
        .await;
    if result.token.is_some() {
        record_turn_stop_tombstone(channel_id, session.as_deref(), reason).await;
    }
    result
}

pub(super) async fn mailbox_cancel_active_turn_with_reason(
    shared: &SharedData,
    channel_id: ChannelId,
    reason: &str,
) -> CancelActiveTurnResult {
    guarded_cancel(shared, channel_id, None, reason).await
}

pub(crate) async fn mailbox_cancel_active_turn_if_handoff_user_message_with_reason(
    shared: &SharedData,
    channel_id: ChannelId,
    handoff_message_id: MessageId,
    reason: &str,
) -> CancelActiveTurnResult {
    guarded_cancel(shared, channel_id, Some(handoff_message_id), reason).await
}

pub(super) async fn active_native(
    shared: &SharedData,
    channel: ChannelId,
    message: Option<MessageId>,
) -> bool {
    let Some(handle) = shared.mailbox_peek(channel) else {
        return false;
    };
    let snapshot = handle.snapshot().await;
    if message.is_some() && snapshot.active_user_message_id != message {
        return false;
    }
    snapshot
        .cancel_token
        .and_then(|token| token.tmux_session_name())
        .is_some_and(|session| super::turn_bridge::native_preserve_stop(&shared.provider, &session))
}
