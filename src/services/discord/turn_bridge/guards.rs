use super::*;

// Guard: ensure completion_tx fires even if the task panics or
// exits early, preventing the parent from hanging on completion_rx.
//
// #2448: also publish an explicit `InflightSignal::Completed`
// broadcast on drop so any per-turn relay tasks (currently the
// standby JSONL relay) can exit immediately instead of polling
// against a wall-clock deadline. The broadcast send is best-effort
// — if no subscriber is registered, `send` returns Err and we
// ignore it.
pub(super) struct CompletionGuard {
    tx: Option<tokio::sync::oneshot::Sender<()>>,
    broadcaster: tokio::sync::broadcast::Sender<super::super::inflight::InflightSignal>,
    channel_id: ChannelId,
    turn_id: u64,
}

impl Drop for CompletionGuard {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(());
        }
        let _ = self
            .broadcaster
            .send(super::super::inflight::InflightSignal::Completed {
                channel_id: self.channel_id.get(),
                turn_id: self.turn_id,
            });
    }
}

// Guard: ensure inflight state file is cleaned up even if the task
// panics or exits early.  On the normal path we defuse the guard
// after the explicit clear_inflight_state() call.
//
// #3161 (codex P2): the Drop runs on ANY abnormal exit (panic / early
// return after the mailbox release but before the explicit defuse). A
// plain unconditional `clear_inflight_state` here is identity-blind and
// can delete a row this turn does NOT own — e.g. a NEWER turn already
// re-wrote the channel's inflight after this turn released the mailbox.
// The guard now carries THIS turn's `user_msg_id` and routes the
// abnormal-path clear through the identity-aware guarded clears, so it
// only removes the row when the on-disk identity still matches THIS
// turn (non-zero) or is a genuine zero-id-owned row (zero). A newer
// owner yields `UserMsgMismatch` and is preserved.
pub(super) struct InflightCleanupGuard {
    provider: Option<ProviderKind>,
    channel_id: u64,
    user_msg_id: u64,
    token_hash: String,
}

pub(super) fn make_bridge_guards(
    bridge: &TurnBridgeContext,
    shared_owned: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    user_msg_id: Option<MessageId>,
) -> (CompletionGuard, InflightCleanupGuard) {
    let completion_guard = CompletionGuard {
        tx: bridge.completion_tx,
        broadcaster: shared_owned.inflight_signals.clone(),
        channel_id,
        turn_id: bridge.inflight_state.effective_finalizer_turn_id(),
    };
    let inflight_guard = InflightCleanupGuard {
        provider: Some(provider.clone()),
        channel_id: channel_id.get(),
        user_msg_id: user_msg_id.map(|id| id.get()).unwrap_or(0),
        token_hash: shared_owned.token_hash.clone(),
    };
    (completion_guard, inflight_guard)
}

impl Drop for InflightCleanupGuard {
    fn drop(&mut self) {
        if let Some(ref provider) = self.provider {
            // #3859: this Drop runs on ANY abnormal exit (panic /
            // early-return) while the turn may still own a live
            // "🔄 처리 중" placeholder. Route through the abandon-request
            // helper — identical ownership guards to the plain guarded
            // clear, but it durably records the placeholder for the
            // placeholder sweeper to finalize to "중단됨" BEFORE deleting
            // the row (which still frees the channel immediately).
            if self.user_msg_id != 0 {
                super::super::inflight::request_inflight_abandon_if_matches(
                    provider,
                    self.channel_id,
                    self.user_msg_id,
                    &self.token_hash,
                );
            } else {
                super::super::inflight::request_inflight_abandon_if_matches_zero_owned(
                    provider,
                    self.channel_id,
                    &self.token_hash,
                );
            }
        }
    }
}
