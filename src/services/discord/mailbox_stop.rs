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
    let native_scope = token.herdr_interrupt_state().is_none()
        && session
            .as_deref()
            .is_some_and(|session| turn_bridge::native_preserve_stop(&shared.provider, session));
    if let Some(session) = session.as_deref()
        && let Some(stop) =
            NativeStop::begin(shared, &shared.provider, channel, &token, session, reason).await
    {
        stop.notice(shared).await;
        return stop.finish().await;
    }
    let result = if native_scope {
        use crate::services::provider::cancel_token_claude_interrupt::StopCancel;
        match handle
            .cancel_active_turn_if_current_unless_herdr(token, reason.into())
            .await
        {
            StopCancel::Published(token) => CancelActiveTurnResult {
                token: Some(token),
                already_stopping: false,
            },
            StopCancel::AlreadyStopping(token) => CancelActiveTurnResult {
                token: Some(token),
                already_stopping: true,
            },
            StopCancel::NotCurrent | StopCancel::Herdr(_) => turn_bridge::empty_native_stop(),
        }
    } else {
        handle
            .cancel_active_turn_if_current_with_reason(token, reason.into())
            .await
    };
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
    // Issue #2374 — the reason-write and the `cancelled` flip are now
    // performed atomically by the mailbox actor (see
    // `ChannelMailboxMsg::CancelActiveTurnWithReason`). The previous
    // design (introduced by PR #2373 for issue #2335) read the active
    // token from the actor, wrote `cancel_source` on it from the caller
    // task, then sent the actor a `CancelActiveTurn`. That kept the
    // ordering correct for a single canceller but allowed two concurrent
    // cancellers to both fetch the same token, race on
    // `set_cancel_source`, and lose one of the reasons. Owning the write
    // inside the actor serializes both transitions per channel and
    // removes the small ordering window.
    let result = shared
        .mailbox(channel_id)
        .cancel_active_turn_with_reason(reason.into())
        .await;
    if result.token.is_some() {
        // #2549: in-memory publish remains immediate, while the matching PG
        // mirror is awaited before the cancel path returns so a quick
        // dcserver restart cannot drop the durable tombstone.
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

/// #2374 Codex round-1 fix (HIGH-1) — identity-guarded variant for the
/// voice handoff cancel path. Cancels the active turn on `channel_id`
/// ONLY when its `user_message_id` matches `handoff_message_id`. An
/// unguarded cancel from the tombstone retry path could otherwise kill
/// an unrelated turn that happened to start on the same target channel
/// after the original handoff turn finalized.
///
/// Recording the tombstone is the caller's responsibility (see
/// [`record_voice_handoff_cancel_tombstone`]) so a tombstone can be
/// written even when no active turn is present (HIGH-2 fix).
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
        .and_then(|token| {
            token.tmux_session_name().or_else(|| {
                inflight::load_inflight_state_read_only(&shared.provider, channel.get())
                    .and_then(|row| row.tmux_session_name)
            })
        })
        .is_some_and(|session| super::turn_bridge::native_preserve_stop(&shared.provider, &session))
}
