//! A channel's queue purge: the forced cancel's whole-queue purge, and the handle requests that
//! report an actor that never answered instead of an empty answer.

use super::*;

/// The `PurgeQueue` arm, which also releases an already-cancelled anchor when asked.
pub(super) fn purge_queue(
    mut state: ChannelMailboxState,
    channel_id: ChannelId,
    persistence: QueuePersistenceContext,
    clear_cancelled_active_anchor: bool,
    reply: oneshot::Sender<PurgeQueueResult>,
) -> ChannelMailboxState {
    if let Err(error) = queued_head::refuse_destructive(&mut state, channel_id, &persistence) {
        tracing::warn!(%channel_id, %error, "queued head refused mailbox purge");
        let _ = reply.send(PurgeQueueResult {
            queued_head_refused: true,
            own_files_removed: Some(0),
            queue_len_after: state.intervention_queue.len(),
            ..Default::default()
        });
        return state;
    }
    // Queue only: a turn that entered between the force-kill and this purge keeps its anchor;
    // a force purge also releases an anchor whose token the kill already cancelled.
    let cleared_active_anchor = if clear_cancelled_active_anchor
        && state
            .cancel_token
            .as_ref()
            .is_some_and(|token| token.cancelled.load(std::sync::atomic::Ordering::Relaxed))
    {
        release_active_turn_anchor(&mut state, channel_id, Some(&persistence.provider));
        true
    } else {
        false
    };
    state.last_persistence = Some(persistence.clone());
    let disk_files_removed =
        remove_channel_pending_queue_files_all_tokens(&persistence.provider, channel_id);
    let own_files_present =
        channel_queue_files_present(&persistence.provider, &persistence.token_hash, channel_id);
    let previous_queue = state.intervention_queue.clone();
    let keep = queued_head::protected_len(&mut state, channel_id, &persistence);
    let drained = state.intervention_queue.drain(keep..).count();
    let purge_persisted = persist_queue_or_restore(
        &mut state,
        channel_id,
        &persistence,
        previous_queue,
        "purge_queue",
    )
    .is_ok();
    let drained = if purge_persisted { drained } else { 0 };
    if purge_persisted {
        clear_pending_user_dispatch(&mut state);
        state.recently_valve_cleared_dispatch = None;
        delete_pending_dispatch_marker_with_persistence(&persistence, channel_id, "purge_queue");
    }
    if cleared_active_anchor {
        mark_turn_finished_signal_done(channel_id);
    }
    let _ = reply.send(PurgeQueueResult {
        drained,
        disk_files_removed,
        cleared_active_anchor,
        own_files_removed: if purge_persisted {
            own_files_present
        } else {
            Some(0)
        },
        queue_len_after: state.intervention_queue.len(),
        queued_head_refused: false,
    });
    state
}

impl ChannelMailboxHandle {
    /// Like `snapshot`, but an actor that never answered is an error, not an empty queue.
    pub(crate) async fn try_snapshot(&self) -> Result<ChannelMailboxSnapshot, MailboxUnreachable> {
        self.request(|reply| ChannelMailboxMsg::Snapshot { reply })
            .await
    }

    /// Like `purge_queue`, but an actor that never answered is an error, not zero counts.
    pub(crate) async fn try_purge_queue(
        &self,
        persistence: QueuePersistenceContext,
        clear_cancelled_active_anchor: bool,
    ) -> Result<PurgeQueueResult, MailboxUnreachable> {
        self.request(|reply| ChannelMailboxMsg::PurgeQueue {
            persistence,
            clear_cancelled_active_anchor,
            reply,
        })
        .await
    }
}
