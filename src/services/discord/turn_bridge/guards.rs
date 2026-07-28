use super::inflight::RelayOwnerKind;
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
    turn_finalizer: Arc<super::super::turn_finalizer::TurnFinalizer>,
    shared: Arc<SharedData>,
    turn_key: super::super::turn_finalizer::TurnKey,
    publish_completed_on_drop: bool,
}

impl CompletionGuard {
    pub(super) fn note_terminal_projection_settled(&self, allow_queue: bool) {
        self.turn_finalizer.note_terminal_projection_settled(
            self.turn_key,
            allow_queue,
            self.shared.clone(),
        );
    }

    pub(super) fn note_terminal_disposition_settled(&self, allow_queue: bool) {
        self.turn_finalizer.note_terminal_disposition_settled(
            self.turn_key,
            allow_queue,
            self.shared.clone(),
        );
    }

    /// Signals the bridge waiter without publishing a terminal event after
    /// durable authority moved to another relay. Publishing `Completed` here
    /// would stop the relay that just became authoritative for the same turn.
    pub(super) fn relinquish_bridge_authority(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(());
        }
        self.publish_completed_on_drop = false;
    }

    #[cfg(test)]
    pub(super) fn for_completion_test(
        shared: Arc<SharedData>,
        channel_id: ChannelId,
        turn_id: u64,
    ) -> Self {
        let (tx, _rx) = tokio::sync::oneshot::channel();
        Self {
            tx: Some(tx),
            broadcaster: shared.inflight_signals.clone(),
            turn_finalizer: shared.turn_finalizer.clone(),
            turn_key: super::super::turn_finalizer::TurnKey::new(
                channel_id,
                turn_id,
                shared.restart.current_generation,
            ),
            shared,
            publish_completed_on_drop: true,
        }
    }
}

impl Drop for CompletionGuard {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(());
        }
        if self.publish_completed_on_drop {
            let _ = self
                .broadcaster
                .send(super::super::inflight::InflightSignal::Completed {
                    channel_id: self.turn_key.channel_id.get(),
                    turn_id: self.turn_key.user_msg_id,
                });
        }
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
    pub(super) provider: Option<ProviderKind>,
    channel_id: u64,
    user_msg_id: u64,
    token_hash: String,
}

impl InflightCleanupGuard {
    /// Disarms abnormal-exit cleanup after the caller explicitly handled or
    /// deliberately preserved the durable row.
    pub(super) fn defuse(&mut self) {
        self.provider.take();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BridgeGuardAuthority {
    channel_id: ChannelId,
    finalizer_turn_id: u64,
    relay_owner: RelayOwnerKind,
    cleanup_user_msg_id: u64,
}

fn bridge_guard_authority(authoritative_state: &InflightTurnState) -> BridgeGuardAuthority {
    BridgeGuardAuthority {
        channel_id: ChannelId::new(authoritative_state.channel_id),
        finalizer_turn_id: authoritative_state.effective_finalizer_turn_id(),
        relay_owner: authoritative_state.effective_relay_owner_kind(),
        cleanup_user_msg_id: authoritative_state.user_msg_id,
    }
}

pub(super) fn make_bridge_guards(
    bridge: &mut TurnBridgeContext,
    authoritative_state: &InflightTurnState,
    shared_owned: &Arc<SharedData>,
    provider: &ProviderKind,
) -> (CompletionGuard, InflightCleanupGuard) {
    let authority = bridge_guard_authority(authoritative_state);
    let key = super::super::turn_finalizer::TurnKey::new(
        authority.channel_id,
        authority.finalizer_turn_id,
        shared_owned.restart.current_generation,
    );
    shared_owned
        .turn_finalizer
        .register_start_with_completion_admission(
            key,
            provider.clone(),
            authority.relay_owner,
            super::super::turn_finalizer::CompletionAdmissionPlan::AfterTerminalProjectionAndDispositionSettled,
            shared_owned,
        );
    let completion_guard = CompletionGuard {
        tx: bridge.completion_tx.take(),
        broadcaster: shared_owned.inflight_signals.clone(),
        turn_finalizer: shared_owned.turn_finalizer.clone(),
        shared: shared_owned.clone(),
        turn_key: key,
        publish_completed_on_drop: true,
    };
    let inflight_guard = InflightCleanupGuard {
        provider: Some(provider.clone()),
        channel_id: authority.channel_id.get(),
        user_msg_id: authority.cleanup_user_msg_id,
        token_hash: shared_owned.token_hash.clone(),
    };
    (completion_guard, inflight_guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merged_id_zero_state_drives_guard_key_owner_and_cleanup_identity() {
        let mut merged = InflightTurnState::new(
            ProviderKind::Codex,
            42_590_701,
            Some("adk-4259-r7".to_string()),
            1,
            0,
            901,
            "synthetic prompt".to_string(),
            Some("merged-session".to_string()),
            Some("AgentDesk-r7-guard".to_string()),
            Some("/tmp/r7-guard.jsonl".to_string()),
            Some("/tmp/r7-guard.input".to_string()),
            512,
        );
        merged.injected_prompt_message_id = Some(77_071);
        merged.set_relay_owner_kind(RelayOwnerKind::Watcher);

        assert_eq!(
            bridge_guard_authority(&merged),
            BridgeGuardAuthority {
                channel_id: ChannelId::new(42_590_701),
                finalizer_turn_id: 77_071,
                relay_owner: RelayOwnerKind::Watcher,
                cleanup_user_msg_id: 0,
            }
        );
    }
}

// #3041 P1-2 (codex P1-a): resolve the AUTHORITATIVE owner channel for
// this turn's tmux session BEFORE the watcher availability check and the
// bridge delivery-lease acquisition. A RECOVERED/restored bridge that
// REUSES an existing watcher (without going through the
// `TmuxReady`/`RuntimeReady` claim paths, which set
// `watcher_owner_channel_id = claim.owner_channel_id()`) would otherwise
// keep `watcher_owner_channel_id == channel_id` (the bridge's dispatch
// channel Y) while the reused watcher leases + advances on its owner
// channel X — different cells, so both could acquire and deliver
// (duplicate). Resolving the session's owner channel here makes EVERY
// path (normal, claim, recovered/restored) key the availability check
// AND the lease acquire+advance on the SAME channel the watcher uses.
// When no reused watcher owns the session, this falls back to
// `channel_id` (the bridge owns its own channel). The claim paths below
// still re-assert `claim.owner_channel_id()` (which equals this resolved
// value for the same session) so live truth always wins.
pub(super) fn resolve_guard_owner_channel(
    shared_owned: &SharedData,
    bridge: &TurnBridgeContext,
) -> ChannelId {
    resolve_bridge_owner_channel(
        &shared_owned.tmux_watchers,
        bridge.inflight_state.tmux_session_name.as_deref(),
        bridge.channel_id,
    )
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
