//! Raw mailbox cancel used by session restart.

use super::*;

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
